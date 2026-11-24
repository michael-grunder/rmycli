//! Hidden terminal prompts, marshalled to the editor thread while it is active.
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use zeroize::Zeroizing;

pub struct Request {
    pub create: bool,
    pub reply: Sender<io::Result<Zeroizing<String>>>,
}

pub fn request(
    channel: Option<&Sender<Request>>,
    create: bool,
) -> io::Result<Zeroizing<String>> {
    let Some(channel) = channel else {
        return master_passphrase(create);
    };
    let (reply, response) = mpsc::channel();
    channel
        .send(Request { create, reply })
        .map_err(|_| cancelled())?;
    response.recv().map_err(|_| cancelled())?
}

pub fn channel() -> (Sender<Request>, Receiver<Request>) {
    mpsc::channel()
}

pub fn master_passphrase(create: bool) -> io::Result<Zeroizing<String>> {
    if !create {
        return read("Master passphrase: ");
    }
    let first = read("New master passphrase: ")?;
    let second = read("Confirm master passphrase: ")?;
    if first.is_empty() {
        return Err(io::Error::other("master passphrase must not be empty"));
    }
    if first != second {
        return Err(io::Error::other("master passphrases do not match"));
    }
    Ok(first)
}

/// Ask a yes/no question on the terminal; Enter accepts `default` and
/// end of input declines.
pub fn confirm(question: &str, default: bool) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "confirmation requires a terminal on stdin",
        ));
    }
    let choices = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        eprint!("{question} {choices} ");
        io::stderr().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(false);
        }
        if let Some(answer) = answer(&line, default) {
            return Ok(answer);
        }
        eprintln!("Please answer yes or no.");
    }
}

fn answer(line: &str, default: bool) -> Option<bool> {
    match line.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "hidden input cancelled")
}

pub fn read(label: &str) -> io::Result<Zeroizing<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    struct RawMode(bool);
    impl Drop for RawMode {
        fn drop(&mut self) {
            if !self.0 {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "hidden input requires a terminal on stdin; use REDISCLI_AUTH for pipes",
        ));
    }
    let guard = RawMode(crossterm::terminal::is_raw_mode_enabled()?);
    crossterm::terminal::enable_raw_mode()?;
    eprint!("{label}");
    io::stderr().flush()?;
    // Fixed capacity avoids leaving previous allocations behind while typing.
    let mut secret = Zeroizing::new(String::with_capacity(1024));
    let result = loop {
        match crossterm::event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match key.code {
                    KeyCode::Enter => break Ok(secret),
                    KeyCode::Esc => break Err(cancelled()),
                    KeyCode::Char('c' | 'd')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        break Err(cancelled());
                    }
                    KeyCode::Backspace => {
                        secret.pop();
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if secret.len() + ch.len_utf8() > 1024 {
                            break Err(io::Error::other(
                                "passphrase exceeds 1024 bytes",
                            ));
                        }
                        secret.push(ch);
                    }
                    _ => {}
                }
            }
            Event::Paste(mut text) => {
                use zeroize::Zeroize;
                let too_long = secret.len() + text.len() > 1024;
                if !too_long {
                    secret.push_str(&text);
                }
                text.zeroize();
                if too_long {
                    break Err(io::Error::other(
                        "passphrase exceeds 1024 bytes",
                    ));
                }
            }
            _ => {}
        }
    };
    eprint!("\r\n");
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::answer;

    #[test]
    fn confirmation_answers_use_the_default_only_for_empty_input() {
        assert_eq!(answer("\n", true), Some(true));
        assert_eq!(answer("  \n", false), Some(false));
        for yes in ["y", "Y", "yes", " YES \n"] {
            assert_eq!(answer(yes, false), Some(true));
        }
        for no in ["n", "No", "no\n"] {
            assert_eq!(answer(no, true), Some(false));
        }
        for other in ["maybe", "ye", "yess", "0"] {
            assert_eq!(answer(other, true), None);
        }
    }
}
