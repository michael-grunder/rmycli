use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

use redis::Value;

use crate::estimate;
use crate::metadata::Catalog;
use crate::scan::{self, Discovery, Limits};
use crate::session::{Config, Session, command};

pub enum Execution {
    Run(Vec<Vec<u8>>),
    Reconnect,
}

pub enum Assistance {
    Scan {
        generation: u64,
        config: Config,
        prefix: Vec<u8>,
        kind: Option<&'static str>,
        state: Discovery,
        limits: Limits,
        probe: Option<(Vec<Vec<u8>>, usize)>,
    },
    Estimate {
        generation: u64,
        config: Config,
        args: Vec<Vec<u8>>,
    },
    Cancel {
        config: Config,
        client_id: i64,
    },
}

pub enum Event {
    Executed {
        lines: Vec<String>,
        config: Config,
        allowed: bool,
        streaming: bool,
        client_id: Option<i64>,
        connected: bool,
    },
    Connected {
        catalog: Box<Catalog>,
        config: Config,
        client_id: Option<i64>,
    },
    Output(Vec<String>),
    Push(Vec<String>),
    Scanned {
        generation: u64,
        state: Discovery,
    },
    Estimated {
        generation: u64,
        count: Option<u64>,
    },
    Failed(String),
    Notice(String),
}

pub struct Workers {
    pub execution: tokio::sync::mpsc::UnboundedSender<Execution>,
    pub assistance: Sender<Assistance>,
    pub events: Receiver<Event>,
    pub generation: Arc<AtomicU64>,
}

async fn client_id(session: &mut Session) -> Option<i64> {
    let result = tokio::time::timeout(
        session.config.timeout,
        session.connection.query(redis::cmd("CLIENT").arg("ID")),
    )
    .await;
    match result {
        Ok(Ok(value)) => redis::from_redis_value(value).ok(),
        Ok(Err(_)) => None,
        Err(_) => {
            session.connection.close();
            None
        }
    }
}

impl Workers {
    pub fn start(session: Session) -> Self {
        let (event_tx, events) = mpsc::sync_channel(64);
        let (execution, execution_rx) = tokio::sync::mpsc::unbounded_channel();
        let (assistance, assistance_rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let assist_generation = Arc::clone(&generation);
        let assist_tx = event_tx.clone();
        thread::spawn(move || {
            assist_loop(&assistance_rx, &assist_tx, &assist_generation);
        });
        let runtime = tokio::runtime::Handle::current();
        thread::spawn(move || {
            runtime.block_on(execution_loop(session, execution_rx, &event_tx));
        });
        Self {
            execution,
            assistance,
            events,
            generation,
        }
    }

    pub fn invalidate(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

fn assist_loop(
    receiver: &Receiver<Assistance>,
    events: &SyncSender<Event>,
    generation: &AtomicU64,
) {
    while let Ok(request) = receiver.recv() {
        match request {
            Assistance::Scan {
                generation: expected,
                config,
                prefix,
                kind,
                state,
                limits,
                probe,
            } => {
                let cancelled =
                    || generation.load(Ordering::Relaxed) != expected;
                if cancelled() {
                    continue;
                }
                let state = match config.connect_auxiliary() {
                    Ok(mut connection) => {
                        if probe.as_ref().is_some_and(|(args, index)| {
                            crate::metadata::probe_key_position(
                                &mut connection,
                                args,
                                *index,
                            ) != Some(true)
                        }) {
                            Discovery { complete: true, notice: "No key position confirmed; complete the arguments and try again".into(), ..state }
                        } else {
                            scan::discover(
                                &mut connection,
                                &prefix,
                                kind,
                                state,
                                &limits,
                                cancelled,
                            )
                        }
                    }
                    Err(error) => Discovery {
                        notice: error.to_string(),
                        ..state
                    },
                };
                let _ = events.send(Event::Scanned {
                    generation: expected,
                    state,
                });
            }
            Assistance::Estimate {
                generation: expected,
                config,
                args,
            } => {
                if generation.load(Ordering::Relaxed) != expected {
                    continue;
                }
                let count = config.connect_auxiliary().ok().and_then(
                    |mut connection| estimate::estimate(&mut connection, &args),
                );
                let _ = events.send(Event::Estimated {
                    generation: expected,
                    count,
                });
            }
            Assistance::Cancel { config, client_id } => {
                let result =
                    config.connect_auxiliary().and_then(|mut connection| {
                        command(&[
                            b"CLIENT".to_vec(),
                            b"KILL".to_vec(),
                            b"ID".to_vec(),
                            client_id.to_string().into_bytes(),
                        ])
                        .query::<i64>(&mut connection)
                    });
                if let Err(error) = result {
                    let _ = events.send(Event::Notice(format!(
                        "Cannot cancel connection: {error}. Ctrl-D exits."
                    )));
                }
            }
        }
    }
}

async fn announce(
    session: &mut Session,
    events: &SyncSender<Event>,
) -> Option<i64> {
    let id = client_id(session).await;
    let catalog = session.config.connect_auxiliary().map_or_else(
        |error| {
            let mut catalog = Catalog::bundled();
            catalog
                .notices
                .push(format!("Metadata connection failed: {error}"));
            catalog
        },
        |mut connection| Catalog::load(&mut connection),
    );
    let _ = events.send(Event::Connected {
        catalog: Box::new(catalog),
        config: session.config.clone(),
        client_id: id,
    });
    id
}

fn received_message(
    session: &mut Session,
    events: &SyncSender<Event>,
    reply: redis::RedisResult<Value>,
) -> bool {
    match reply {
        Ok(reply) => {
            let push = matches!(reply, Value::Push { .. });
            if !push || session.streaming || session.config.show_pushes {
                let lines = session.config.output.lines(&reply);
                let event = if push {
                    Event::Push(lines)
                } else {
                    Event::Output(lines)
                };
                return events.send(event).is_ok();
            }
        }
        Err(error) => {
            let state = if session.streaming {
                "Stream ended"
            } else {
                "Connection lost"
            };
            session.streaming = false;
            let _ = events.send(Event::Failed(format!(
                "{state}: {error}. Use \\reconnect."
            )));
            return false;
        }
    }
    true
}

async fn execution_loop(
    mut session: Session,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Execution>,
    events: &SyncSender<Event>,
) {
    let mut id = announce(&mut session, events).await;
    let mut listening = true;
    loop {
        let request = tokio::select! {
            request = requests.recv() => match request {
                Some(request) => request,
                None => return,
            },
            reply = session.connection.recv_response(), if listening => {
                listening = received_message(&mut session, events, reply);
                continue;
            }
        };
        match request {
            Execution::Run(args) => {
                let previous_addr = session.config.info.addr().clone();
                let output = session.config.output.clone();
                let show_pushes = session.config.show_pushes;
                let result = session
                    .execute_with_push(&args, |reply| {
                        if show_pushes {
                            let _ =
                                events.send(Event::Push(output.lines(reply)));
                        }
                    })
                    .await;
                if previous_addr != *session.config.info.addr() {
                    id = client_id(&mut session).await;
                }
                let connected = result
                    .as_ref()
                    .err()
                    .is_none_or(|error| !error.is_io_error());
                listening = connected;
                let lines = result.map_or_else(
                    |error| {
                        vec![format!(
                            "(error) {}",
                            crate::output::error_message(&error)
                        )]
                    },
                    |reply| output.lines(&reply),
                );
                if events
                    .send(Event::Executed {
                        lines,
                        config: session.config.clone(),
                        allowed: session.assistance_allowed() && connected,
                        streaming: session.streaming,
                        client_id: id,
                        connected,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Execution::Reconnect => {
                match Session::open(session.config.clone()).await {
                    Ok(fresh) => {
                        session = fresh;
                        listening = true;
                        id = announce(&mut session, events).await;
                    }
                    Err(error) => {
                        let _ = events.send(Event::Failed(format!(
                            "Reconnect failed: {error}"
                        )));
                    }
                }
            }
        }
    }
}
