#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

mod auth;
mod batch;
mod cli;
mod config;
mod config_cli;
mod estimate;
mod grammar;
mod hints;
mod history;
mod input;
mod metadata;
mod output;
mod scan;
mod session;
mod terminal;
mod transport;
mod worker;

#[cfg(test)]
mod integration_tests;

use std::io::{self, IsTerminal};
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, parser::ValueSource};
use cli::Options;
use session::Session;

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() {
    let matches = Options::command().get_matches();
    let options = Options::from_arg_matches(&matches)
        .unwrap_or_else(|error| error.exit());
    let explicit_credentials = matches.value_source("password")
        == Some(ValueSource::CommandLine)
        || matches.value_source("uri") == Some(ValueSource::CommandLine)
            && options.uri.as_ref().is_some_and(|uri| uri.contains('@'));
    if let Err(error) = run(&options, explicit_credentials).await {
        let message = error
            .downcast_ref::<redis::RedisError>()
            .map_or_else(|| error.to_string(), output::error_message);
        eprintln!("rmycli: {message}");
        std::process::exit(1);
    }
}

async fn run(
    options: &Options,
    explicit_credentials: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if options.batch.eval.is_none()
        && options
            .command
            .first()
            .is_some_and(|command| command == "config")
    {
        return config_cli::run(
            &options.command,
            options.settings.config.as_deref(),
            options.settings.no_config,
        );
    }
    let settings = config::Settings::load(
        options.settings.config.as_deref(),
        options.settings.no_config,
    )?;
    let mut config = options.config(&settings)?;
    if let Some(user) = &options.set_default_user {
        let store = auth::Store::discover(settings.save_auth)
            .ok_or("cannot locate the user config directory")?;
        store.set_default_user(config.info.addr(), user)?;
        println!("Default user set to '{}'", user.escape_default());
        return Ok(());
    }
    let args = batch::stdin_command(options)?;
    if !options.settings.no_config {
        config.auth_store = auth::Store::discover(settings.save_auth);
        if let Some(store) = &config.auth_store {
            store.apply(&mut config.info).map_err(|error| {
                format!(
                    "cannot load auth file {}: {error}",
                    store.path.display()
                )
            })?;
        }
    }
    if explicit_credentials && !options.no_auth_warning && !options.askpass {
        eprintln!(
            "Warning: command-line passwords may be visible to other users. Prefer REDISCLI_AUTH or --askpass."
        );
    }
    let mut session = Session::open(config).await?;
    for notice in session.notices.drain(..) {
        eprintln!("{notice}");
    }
    if options.verbose {
        eprintln!(
            "Connected to {} (database {}, {:?})",
            session.config.info.addr(),
            session.config.info.redis_settings().db(),
            session.config.info.redis_settings().protocol()
        );
    }
    if options.batch.scan {
        batch::scan(&mut session, options).await?;
    } else if !args.is_empty() {
        batch::execute(&mut session, &args, options).await?;
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        terminal::run(
            session,
            terminal::Options {
                limits: scan::Limits {
                    threshold: options.completion_threshold,
                    keys: options.scan_max_keys,
                    bytes: options.scan_max_bytes,
                    batches: options.scan_max_batches,
                    time: Duration::from_millis(options.scan_ms),
                },
                reply_threshold: options.reply_threshold,
                yes: options.yes,
                history_per_server: settings.history_per_server,
                theme: settings.theme,
            },
        )?;
    } else {
        loop {
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                break;
            }
            let line = line.strip_suffix('\n').map_or(line.as_str(), |line| {
                line.strip_suffix('\r').unwrap_or(line)
            });
            let parsed = input::parse(line);
            if let Some(error) = parsed.error {
                return Err(error.into());
            }
            let args = parsed
                .words
                .into_iter()
                .map(|word| word.bytes)
                .collect::<Vec<_>>();
            if !args.is_empty() {
                batch::execute(&mut session, &args, options).await?;
            }
        }
    }
    Ok(())
}

async fn batch_command(
    session: &mut Session,
    args: &[Vec<u8>],
    options: &Options,
) -> Result<(), Box<dyn std::error::Error>> {
    if session.assistance_allowed()
        && !options.yes
        && args[0].eq_ignore_ascii_case(b"LRANGE")
    {
        let estimate = session.config.connect_auxiliary().ok().and_then(
            |mut connection| estimate::estimate(&mut connection, args),
        );
        if estimate::needs_confirmation(
            estimate,
            options.reply_threshold,
            false,
        ) {
            return Err(format!("reply estimated at {} elements; not executed. Use --yes to allow it in noninteractive mode",
                estimate.unwrap_or_default()).into());
        }
    }
    let show_pushes = session.config.show_pushes;
    let mut output_error = None;
    let reply = session
        .execute_with_push(args, |reply| {
            if show_pushes && output_error.is_none() {
                output_error = options.output.print(reply).err();
            }
        })
        .await?;
    for notice in session.notices.drain(..) {
        eprintln!("{notice}");
    }
    if let Some(error) = output_error {
        return Err(error.into());
    }
    options.output.print(&reply)?;
    while session.streaming {
        options
            .output
            .print(&session.connection.recv_response().await?)?;
    }
    Ok(())
}
