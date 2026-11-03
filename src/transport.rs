//! Raw async replies preserve streaming commands and protocol changes on one socket.
use std::io;
use std::sync::Arc;
use std::time::Duration;

use redis::{ConnectionAddr, RedisResult, Value};
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::session::Config;

type Stream = Box<dyn redis::aio::AsyncStream + Unpin + Send>;

pub struct Connection {
    writer: Option<WriteHalf<Stream>>,
    incoming: mpsc::Receiver<RedisResult<Value>>,
    reader: JoinHandle<()>,
    write_timeout: Duration,
}

fn closed() -> redis::RedisError {
    io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed").into()
}

impl Connection {
    pub async fn open(config: &Config) -> RedisResult<Self> {
        let connect = async {
            let stream: Stream = match config.info.addr() {
                ConnectionAddr::Tcp(host, port) => {
                    let stream =
                        tokio::net::TcpStream::connect((host.as_str(), *port))
                            .await?;
                    stream.set_nodelay(true)?;
                    Box::new(stream)
                }
                ConnectionAddr::TcpTls { host, port, .. } => {
                    let stream =
                        tokio::net::TcpStream::connect((host.as_str(), *port))
                            .await?;
                    stream.set_nodelay(true)?;
                    let tls = config.tls.clone().ok_or_else(|| {
                        io::Error::other("missing TLS configuration")
                    })?;
                    let name =
                        rustls::pki_types::ServerName::try_from(host.clone())?;
                    Box::new(
                        tokio_rustls::TlsConnector::from(tls)
                            .connect(name, stream)
                            .await?,
                    )
                }
                #[cfg(unix)]
                ConnectionAddr::Unix(path) => {
                    Box::new(tokio::net::UnixStream::connect(path).await?)
                }
                _ => {
                    return Err(io::Error::other(
                        "unsupported connection address",
                    )
                    .into());
                }
            };
            let (mut read, writer) = tokio::io::split(stream);
            let (sender, incoming) = mpsc::channel(64);
            let reader = tokio::spawn(async move {
                let mut decoder = combine::stream::Decoder::new();
                loop {
                    let reply = tokio::select! {
                        () = sender.closed() => break,
                        reply = redis::parse_redis_value_async(&mut decoder, &mut read) => reply,
                    };
                    let failed = reply.is_err();
                    if sender.send(reply).await.is_err() || failed {
                        break;
                    }
                }
            });
            let mut connection = Self {
                writer: Some(writer),
                incoming,
                reader,
                write_timeout: config.timeout,
            };
            connection.handshake(config).await?;
            Ok(connection)
        };
        if let Some(timeout) = config.connect_timeout {
            tokio::time::timeout(timeout, connect).await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection setup timed out",
                )
            })?
        } else {
            connect.await
        }
    }

    async fn handshake(&mut self, config: &Config) -> RedisResult<()> {
        let settings = config.info.redis_settings();
        if settings.protocol() == redis::ProtocolVersion::RESP3 {
            let mut hello = redis::cmd("HELLO");
            hello.arg(3);
            if let Some(password) = settings.password() {
                hello
                    .arg("AUTH")
                    .arg(settings.username().unwrap_or("default"))
                    .arg(password);
            }
            self.query(&hello).await?;
        } else if let Some(password) = settings.password() {
            let mut auth = redis::cmd("AUTH");
            if let Some(user) = settings.username() {
                auth.arg(user);
            }
            auth.arg(password);
            let reply = match self.query(&auth).await {
                Err(error)
                    if settings.username().is_some()
                        && error.detail().is_some_and(|detail| {
                            detail.contains(
                                "wrong number of arguments for 'auth' command",
                            )
                        }) =>
                {
                    // Redis before ACLs accepts only AUTH password.
                    self.query(redis::cmd("AUTH").arg(password)).await?
                }
                result => result?,
            };
            if reply != Value::Okay {
                return Err((
                    redis::ErrorKind::AuthenticationFailed,
                    "unexpected AUTH reply",
                )
                    .into());
            }
        }
        if settings.db() != 0 {
            self.query(redis::cmd("SELECT").arg(settings.db())).await?;
        }
        if let Some(name) = &config.name {
            self.query(redis::cmd("CLIENT").arg("SETNAME").arg(name))
                .await?;
        }
        Ok(())
    }

    pub async fn send_packed_command(
        &mut self,
        bytes: &[u8],
    ) -> RedisResult<()> {
        let writer = self.writer.as_mut().ok_or_else(closed)?;
        let result = tokio::time::timeout(self.write_timeout, async {
            writer.write_all(bytes).await?;
            writer.flush().await
        })
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command write timed out",
            ))
        });
        if result.is_err() {
            self.close();
        }
        Ok(result?)
    }

    pub async fn recv_response(&mut self) -> RedisResult<Value> {
        let result =
            self.incoming.recv().await.unwrap_or_else(|| Err(closed()));
        if result.is_err() {
            self.close();
        }
        result
    }

    pub async fn query(&mut self, command: &redis::Cmd) -> RedisResult<Value> {
        self.send_packed_command(&command.get_packed_command())
            .await?;
        loop {
            match self.recv_response().await? {
                Value::Push { .. } => {}
                Value::ServerError(error) => return Err(error.into()),
                value => return Ok(value),
            }
        }
    }

    pub fn close(&mut self) {
        self.writer = None;
        self.reader.abort();
        self.incoming.close();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub fn tls_config(
    certificates: &redis::TlsCertificates,
    insecure: bool,
) -> Result<Arc<rustls::ClientConfig>, Box<dyn std::error::Error>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let mut roots = rustls::RootCertStore::empty();
    if let Some(pem) = &certificates.root_cert {
        for certificate in CertificateDer::pem_slice_iter(pem) {
            roots.add(certificate?)?;
        }
        if roots.is_empty() {
            return Err("CA file contains no certificates".into());
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        if let Some(error) = native.errors.first() {
            return Err(error.to_string().into());
        }
        for certificate in native.certs {
            roots.add(certificate)?;
        }
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let mut config = if let Some(client) = &certificates.client_tls {
        builder.with_client_auth_cert(
            CertificateDer::pem_slice_iter(&client.client_cert)
                .collect::<Result<_, _>>()?,
            PrivateKeyDer::from_pem_slice(&client.client_key)?,
        )?
    } else {
        builder.with_no_client_auth()
    };
    if insecure {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(Insecure));
    }
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct Insecure;
impl rustls::client::danger::ServerCertVerifier for Insecure {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
    {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
    {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
