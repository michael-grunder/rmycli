#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

mod batch;
mod cli;
mod config;
mod config_cli;
mod estimate;
mod grammar;
mod hints;
mod input;
mod metadata;
mod output;
mod scan;
mod session;
mod terminal;
mod worker;

#[cfg(test)]
mod integration_tests;

use std::io::{self, BufRead, IsTerminal};
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, parser::ValueSource};
use cli::Options;
use session::Session;

fn main() {
    let matches = Options::command().get_matches();
    let options = Options::from_arg_matches(&matches)
        .unwrap_or_else(|error| error.exit());
    let explicit_credentials = matches.value_source("password")
        == Some(ValueSource::CommandLine)
        || matches.value_source("uri") == Some(ValueSource::CommandLine)
            && options.uri.as_ref().is_some_and(|uri| uri.contains('@'));
    if let Err(error) = run(&options, explicit_credentials) {
        eprintln!("rmycli: {error}");
        std::process::exit(1);
    }
}

fn run(
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
    let args = batch::stdin_command(options)?;
    let config = options.config(&settings)?;
    if explicit_credentials && !options.no_auth_warning && !options.askpass {
        eprintln!(
            "Warning: command-line passwords may be visible to other users. Prefer REDISCLI_AUTH or --askpass."
        );
    }
    let mut session = Session::open(config)?;
    if options.verbose {
        eprintln!(
            "Connected to {} (database {}, {:?})",
            session.config.info.addr,
            session.config.info.redis.db,
            session.config.info.redis.protocol
        );
    }
    if options.batch.scan {
        batch::scan(&mut session, options)?;
    } else if !args.is_empty() {
        batch::execute(&mut session, &args, options)?;
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
                theme: settings.theme,
            },
        )?;
    } else {
        for line in io::stdin().lock().lines() {
            let line = line?;
            let parsed = input::parse(&line);
            if let Some(error) = parsed.error {
                return Err(error.into());
            }
            let args = parsed
                .words
                .into_iter()
                .map(|word| word.bytes)
                .collect::<Vec<_>>();
            if !args.is_empty() {
                batch::execute(&mut session, &args, options)?;
            }
        }
    }
    Ok(())
}

fn batch_command(
    session: &mut Session,
    args: &[Vec<u8>],
    options: &Options,
) -> Result<(), Box<dyn std::error::Error>> {
    if session.assistance_allowed()
        && !options.yes
        && args[0].eq_ignore_ascii_case(b"LRANGE")
    {
        let estimate =
            session
                .config
                .connect(true)
                .ok()
                .and_then(|mut connection| {
                    estimate::estimate(&mut connection, args)
                });
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
    let reply = session.execute_with_push(args, |reply| {
        if show_pushes && output_error.is_none() {
            output_error = options.output.print(reply).err();
        }
    })?;
    if let Some(error) = output_error {
        return Err(error.into());
    }
    options.output.print(&reply)?;
    while session.streaming {
        options.output.print(&session.connection.recv_response()?)?;
    }
    Ok(())
}
