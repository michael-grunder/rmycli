use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;

use redis::Value;

use crate::estimate;
use crate::metadata::Catalog;
use crate::scan::{self, Discovery, Limits};
use crate::session::{Config, Session};

pub enum Execution {
    Run(Vec<Vec<u8>>),
    Scan {
        pattern: Vec<u8>,
        cancelled: Arc<AtomicBool>,
    },
    Reconnect,
    Auth(crate::auth::Action),
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
    SizeHint {
        generation: u64,
        config: Config,
        line: String,
        command: String,
        args: Vec<Vec<u8>>,
        keys: Vec<Vec<u8>>,
    },
}

pub enum Event {
    Executed {
        lines: Vec<String>,
        config: Config,
        allowed: bool,
        streaming: bool,
        subscribed: bool,
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
    SizeHinted {
        generation: u64,
        line: String,
        hint: Option<estimate::SizeHint>,
    },
    Failed(String),
}

pub struct Workers {
    pub execution: tokio::sync::mpsc::UnboundedSender<Execution>,
    pub assistance: Sender<Assistance>,
    pub events: Receiver<Event>,
    pub generation: Arc<AtomicU64>,
    pub cancel: Arc<tokio::sync::Notify>,
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
        let cancel = Arc::new(tokio::sync::Notify::new());
        let execution_cancel = Arc::clone(&cancel);
        let runtime = tokio::runtime::Handle::current();
        thread::spawn(move || {
            runtime.block_on(execution_loop(
                session,
                execution_rx,
                &event_tx,
                &execution_cancel,
            ));
        });
        Self {
            execution,
            assistance,
            events,
            generation,
            cancel,
        }
    }

    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
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
            Assistance::SizeHint {
                generation: expected,
                config,
                line,
                command,
                args,
                keys,
            } => {
                // Typing queues one request per keystroke; skip stale ones.
                if generation.load(Ordering::Relaxed) != expected {
                    continue;
                }
                let hint = config.connect_auxiliary().ok().and_then(
                    |mut connection| {
                        estimate::size_hint(
                            &mut connection,
                            &command,
                            &args,
                            &keys,
                        )
                    },
                );
                let _ = events.send(Event::SizeHinted {
                    generation: expected,
                    line,
                    hint,
                });
            }
        }
    }
}

async fn announce(
    session: &mut Session,
    events: &SyncSender<Event>,
) -> Option<i64> {
    let id = client_id(session).await;
    let mut catalog = session.config.connect_auxiliary().map_or_else(
        |error| {
            let mut catalog = Catalog::bundled();
            catalog
                .notices
                .push(format!("Metadata connection failed: {error}"));
            catalog
        },
        |mut connection| Catalog::load(&mut connection),
    );
    catalog.notices.append(&mut session.notices);
    let _ = events.send(Event::Connected {
        catalog: Box::new(catalog),
        config: session.config.clone(),
        client_id: id,
    });
    id
}

fn received_message(
    session: &Session,
    events: &SyncSender<Event>,
    reply: &Value,
) -> bool {
    let push = matches!(reply, Value::Push { .. });
    if !push || crate::session::is_pubsub(reply) || session.config.show_pushes {
        let lines = if session.streaming
            && let Value::SimpleString(line) = reply
        {
            vec![line.clone()]
        } else {
            session.config.output.lines(reply)
        };
        let event = if push {
            Event::Push(lines)
        } else {
            Event::Output(lines)
        };
        return events.send(event).is_ok();
    }
    true
}

async fn reconnect(
    session: &mut Session,
    events: &SyncSender<Event>,
    id: &mut Option<i64>,
) -> bool {
    // Drop the old stream before any handshake or metadata requests.
    session.connection.close();
    match Session::open(session.config.clone()).await {
        Ok(fresh) => {
            *session = fresh;
            *id = announce(session, events).await;
            true
        }
        Err(error) => {
            let _ = events.send(Event::Failed(format!(
                "Reconnect failed: {error}. Submit a new command or use \\reconnect to try again."
            )));
            false
        }
    }
}

async fn execution_loop(
    mut session: Session,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Execution>,
    events: &SyncSender<Event>,
    cancel: &tokio::sync::Notify,
) {
    let mut id = announce(&mut session, events).await;
    let mut auth_revision = session.auth_revision;
    let mut listening = true;
    let mut needs_reconnect = false;
    loop {
        if needs_reconnect {
            listening = reconnect(&mut session, events, &mut id).await;
            auth_revision = session.auth_revision;
            needs_reconnect = false;
        }
        let request = tokio::select! {
            biased;
            request = requests.recv() => match request {
                Some(request) => request,
                None => return,
            },
            () = cancel.notified() => Execution::Reconnect,
            reply = session.connection.recv_response(), if listening => {
                match reply {
                    Ok(reply) => {
                        if !received_message(&session, events, &reply) {
                            return;
                        }
                    }
                    Err(_) => needs_reconnect = true,
                }
                continue;
            }
        };
        match request {
            request @ (Execution::Run(_) | Execution::Scan { .. }) => {
                if !listening {
                    listening = reconnect(&mut session, events, &mut id).await;
                    auth_revision = session.auth_revision;
                    if !listening {
                        continue;
                    }
                }
                let previous_addr = session.config.info.addr().clone();
                let result = tokio::select! {
                    biased;
                    () = cancel.notified() => {
                        needs_reconnect = true;
                        continue;
                    }
                    result = execute_request(&mut session, request, events) => result,
                };
                if previous_addr != *session.config.info.addr() {
                    id = client_id(&mut session).await;
                }
                let connected = result
                    .as_ref()
                    .err()
                    .is_none_or(|error| !error.is_io_error());
                listening = connected;
                needs_reconnect = !connected;
                let mut lines = result.unwrap_or_else(|error| {
                    vec![format!(
                        "(error) {}",
                        crate::output::error_message(&error)
                    )]
                });
                lines.append(&mut session.notices);
                if events
                    .send(executed(&session, lines, id, connected))
                    .is_err()
                {
                    return;
                }
                if connected
                    && session.assistance_allowed()
                    && auth_revision != session.auth_revision
                {
                    id = announce(&mut session, events).await;
                    auth_revision = session.auth_revision;
                }
            }
            Execution::Auth(action) => {
                credential_action(&session, action, events, id, listening);
            }
            Execution::Reconnect => needs_reconnect = true,
        }
    }
}

async fn execute_request(
    session: &mut Session,
    request: Execution,
    events: &SyncSender<Event>,
) -> redis::RedisResult<Vec<String>> {
    let output = session.config.output.clone();
    let show_pushes = session.config.show_pushes;
    let push = |reply: &Value| {
        if show_pushes || crate::session::is_pubsub(reply) {
            let _ = events.send(Event::Push(output.lines(reply)));
        }
    };
    match request {
        Execution::Run(args) => session
            .execute_with_push(&args, push)
            .await
            .map(|reply| output.lines(&reply)),
        Execution::Scan { pattern, cancelled } => {
            scan_keys(session, &pattern, &cancelled, events, push)
                .await
                .map(|()| Vec::new())
        }
        Execution::Reconnect | Execution::Auth(_) => unreachable!(),
    }
}

fn credential_action(
    session: &Session,
    action: crate::auth::Action,
    events: &SyncSender<Event>,
    id: Option<i64>,
    listening: bool,
) {
    let result = session
        .config
        .auth_store
        .as_ref()
        .ok_or_else(|| {
            std::io::Error::other("saved authentication is disabled")
        })
        .and_then(|store| action.run(store));
    let lines = vec![
        result.map_or_else(|error| format!("(error) {error}"), str::to_owned),
    ];
    let _ = events.send(executed(session, lines, id, listening));
}

fn executed(
    session: &Session,
    lines: Vec<String>,
    client_id: Option<i64>,
    connected: bool,
) -> Event {
    Event::Executed {
        lines,
        config: session.config.clone(),
        allowed: session.assistance_allowed() && connected,
        streaming: session.streaming,
        subscribed: session.subscribed(),
        client_id,
        connected,
    }
}

async fn scan_keys(
    session: &mut Session,
    pattern: &[u8],
    cancelled: &AtomicBool,
    events: &SyncSender<Event>,
    mut push: impl FnMut(&Value),
) -> redis::RedisResult<()> {
    if !session.assistance_allowed() {
        return Err(redis::RedisError::from((
            redis::ErrorKind::Client,
            "Alternate KEYS is unavailable during MULTI, WATCH, or streaming",
        )));
    }
    let mut cursor = 0_u64;
    while !cancelled.load(Ordering::Relaxed) {
        let args = [
            b"SCAN".to_vec(),
            cursor.to_string().into_bytes(),
            b"MATCH".to_vec(),
            pattern.to_vec(),
            b"COUNT".to_vec(),
            b"128".to_vec(),
        ];
        let reply = session.execute_with_push(&args, &mut push).await?;
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::from_redis_value(reply)?;
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let lines = keys
            .into_iter()
            .flat_map(|key| {
                session.config.output.lines(&Value::BulkString(key))
            })
            .collect();
        if events.send(Event::Output(lines)).is_err() {
            break;
        }
        cursor = next;
        if cursor == 0 {
            return Ok(());
        }
    }
    Ok(())
}
