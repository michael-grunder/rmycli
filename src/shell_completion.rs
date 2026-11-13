//! Local, noninteractive value completion for the calling shell.
use std::ffi::{OsStr, OsString};

use clap::Parser;
use clap_complete::CompletionCandidate;

use crate::{auth, cli::Options, config::Settings};

fn context() -> Option<(Options, Settings)> {
    // CompleteEnv forwards the shell's words after its own `--` separator.
    let mut args: Vec<OsString> = std::env::args_os()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect();
    if args.is_empty() {
        return None;
    }
    let index = std::env::var("_CLAP_COMPLETE_INDEX")
        .ok()
        .and_then(|index| index.parse::<usize>().ok())
        .unwrap_or(args.len() - 1);
    // Zsh can omit the empty word at the cursor.
    if index == args.len() {
        args.push(OsString::new());
    }
    let options = Options::try_parse_from(&args).ok()?;
    // Option-looking words after the command belong to Redis.
    if index >= args.len() - options.command.len() {
        return None;
    }
    let settings = Settings::load(
        options.settings.config.as_deref(),
        options.settings.no_config,
    )
    .ok()?;
    Some((options, settings))
}

fn candidates(
    names: impl IntoIterator<Item = String>,
    current: &OsStr,
) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let mut names: Vec<_> = names
        .into_iter()
        // Shell completion protocols use control characters as delimiters.
        .filter(|name| {
            name.starts_with(prefix) && !name.chars().any(char::is_control)
        })
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(CompletionCandidate::new).collect()
}

pub fn instances(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some((_, settings)) = context() else {
        return Vec::new();
    };
    candidates(settings.instances.into_keys(), current)
}

pub fn users(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some((options, settings)) = context() else {
        return Vec::new();
    };
    if options.settings.no_config {
        return Vec::new();
    }
    let Ok(info) = options.connection_info(&settings) else {
        return Vec::new();
    };
    let mut names =
        auth::Store::discover(settings.save_auth, settings.encrypt_auth)
            .ok()
            .flatten()
            .and_then(|store| store.usernames(info.addr()).ok())
            .unwrap_or_default();
    if options.set_default_user.is_none()
        && let Some(user) = options
            .instance(&settings)
            .ok()
            .and_then(|instance| instance.user.as_ref())
    {
        names.push(user.clone());
    }
    candidates(names, current)
}
