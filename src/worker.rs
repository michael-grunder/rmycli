use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

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
    pub execution: Sender<Execution>,
    pub assistance: Sender<Assistance>,
    pub events: Receiver<Event>,
    pub generation: Arc<AtomicU64>,
}

fn client_id(session: &mut Session) -> Option<i64> {
    redis::cmd("CLIENT")
        .arg("ID")
        .query(&mut session.connection)
        .ok()
}

impl Workers {
    pub fn start(session: Session) -> Self {
        let (event_tx, events) = mpsc::sync_channel(64);
        let (execution, execution_rx) = mpsc::channel();
        let (assistance, assistance_rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let assist_generation = Arc::clone(&generation);
        let assist_tx = event_tx.clone();
        thread::spawn(move || {
            assist_loop(&assistance_rx, &assist_tx, &assist_generation);
        });
        thread::spawn(move || {
            execution_loop(session, &execution_rx, &event_tx);
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
                let state = match config.connect(true) {
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
                let count =
                    config.connect(true).ok().and_then(|mut connection| {
                        estimate::estimate(&mut connection, &args)
                    });
                let _ = events.send(Event::Estimated {
                    generation: expected,
                    count,
                });
            }
            Assistance::Cancel { config, client_id } => {
                let result = config.connect(true).and_then(|mut connection| {
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

fn announce(session: &mut Session, events: &SyncSender<Event>) -> Option<i64> {
    let _ = session
        .connection
        .set_read_timeout(Some(session.config.timeout));
    let id = client_id(session);
    let catalog = Catalog::load(&mut session.connection);
    let _ = session.connection.set_read_timeout(None);
    let _ = events.send(Event::Connected {
        catalog: Box::new(catalog),
        config: session.config.clone(),
        client_id: id,
    });
    id
}

fn execution_loop(
    mut session: Session,
    requests: &Receiver<Execution>,
    events: &SyncSender<Event>,
) {
    let mut id = announce(&mut session, events);
    while let Ok(request) = requests.recv() {
        match request {
            Execution::Run(args) => {
                let previous_addr = session.config.info.addr.clone();
                let output = session.config.output.clone();
                let show_pushes = session.config.show_pushes;
                let result = session.execute_with_push(&args, |reply| {
                    if show_pushes {
                        let _ = events.send(Event::Output(output.lines(reply)));
                    }
                });
                if previous_addr != session.config.info.addr {
                    id = client_id(&mut session);
                }
                let connected = result
                    .as_ref()
                    .err()
                    .is_none_or(|error| !error.is_io_error());
                let lines = result.map_or_else(
                    |error| vec![format!("(error) {error}")],
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
                // Streaming reads have no timeout: redis-rs intentionally skips
                // a reply after a read timeout. Ctrl-C closes this exact client
                // via the auxiliary connection, waking the reader safely.
                while session.streaming {
                    match session.connection.recv_response() {
                        Ok(reply) => {
                            if events
                                .send(Event::Output(output.lines(&reply)))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            session.streaming = false;
                            let _ = events.send(Event::Failed(format!(
                                "Stream ended: {error}. Use \\reconnect."
                            )));
                        }
                    }
                }
            }
            Execution::Reconnect => match Session::open(session.config.clone())
            {
                Ok(fresh) => {
                    session = fresh;
                    id = announce(&mut session, events);
                }
                Err(error) => {
                    let _ = events.send(Event::Failed(format!(
                        "Reconnect failed: {error}"
                    )));
                }
            },
        }
    }
}
