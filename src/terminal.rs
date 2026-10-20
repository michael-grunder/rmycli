use std::collections::VecDeque;
use std::io::{self, Write};
use std::ops::Range;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{
        self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use unicode_width::UnicodeWidthChar;

use crate::{
    config::Theme,
    estimate, hints, input,
    metadata::Catalog,
    scan::{Discovery, Limits},
    session::{Config, Session},
    worker::{Assistance, Event, Execution, Workers},
};

struct Screen;
impl Screen {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let screen = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            event::EnableBracketedPaste
        )?;
        Ok(screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            ResetColor,
            cursor::Show,
            event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Default)]
struct Editor {
    line: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
}

impl Editor {
    fn insert(&mut self, text: &str) {
        self.line.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    fn replace(&mut self, span: Range<usize>, text: &str) {
        self.line.replace_range(span.clone(), text);
        self.cursor = span.start + text.len();
    }
    fn left(&mut self) {
        self.cursor = self.line[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }
    fn right(&mut self) {
        self.cursor += self.line[self.cursor..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
    }
    fn remember(&mut self) {
        if self.history.last() != Some(&self.line) {
            self.history.push(self.line.clone());
        }
        if self.history.len() > 500 {
            self.history.remove(0);
        }
        self.history_index = None;
    }
    fn history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.draft.clone_from(&self.line);
        }
        let index = self.history_index.unwrap_or(self.history.len());
        let index = if up {
            index.saturating_sub(1)
        } else {
            (index + 1).min(self.history.len())
        };
        self.line = self.history.get(index).unwrap_or(&self.draft).clone();
        self.history_index = (index < self.history.len()).then_some(index);
        self.cursor = self.line.len();
    }
}

struct Popup {
    candidates: Vec<String>,
    selected: usize,
    span: Range<usize>,
}

impl Popup {
    const fn cycle(&mut self, reverse: bool) {
        let len = self.candidates.len();
        self.selected = if reverse {
            (self.selected + len - 1) % len
        } else {
            (self.selected + 1) % len
        };
    }
    fn accept(self, editor: &mut Editor) {
        editor.replace(self.span, &self.candidates[self.selected]);
    }

    fn layout(
        &self,
        anchor: usize,
        row: usize,
        screen_width: usize,
    ) -> PopupLayout {
        let rows = self.candidates.len().min(8).min(row.saturating_sub(2));
        let longest = self
            .candidates
            .iter()
            .map(|s| display_width(&safe(s)))
            .max()
            .unwrap_or(0);
        let width = (longest + 4).max(14).min(screen_width);
        PopupLayout {
            x: anchor.min(screen_width.saturating_sub(width)),
            y: row.saturating_sub(rows + 2),
            width,
            rows,
            first: self
                .selected
                .saturating_sub(rows / 2)
                .min(self.candidates.len().saturating_sub(rows)),
        }
    }

    fn draw(
        &self,
        out: &mut impl Write,
        anchor: usize,
        row: usize,
        screen_width: usize,
    ) -> io::Result<()> {
        let area = self.layout(anchor, row, screen_width);
        if area.rows == 0 || area.width < 4 {
            return Ok(());
        }
        let title =
            format!(" {}/{} ", self.selected + 1, self.candidates.len());
        let title = fit(&title, area.width - 2);
        let border = format!(
            "┌{title}{}┐",
            "─".repeat(area.width - 2 - display_width(&title))
        );
        popup_line(out, area.x, area.y, &border, false)?;
        for offset in 0..area.rows {
            let index = area.first + offset;
            let label = fit(&safe(&self.candidates[index]), area.width - 4);
            let padding = " ".repeat(area.width - 4 - display_width(&label));
            popup_line(
                out,
                area.x,
                area.y + offset + 1,
                &format!("│ {label}{padding} │"),
                index == self.selected,
            )?;
        }
        popup_line(
            out,
            area.x,
            area.y + area.rows + 1,
            &format!("└{}┘", "─".repeat(area.width - 2)),
            false,
        )?;
        queue!(out, ResetColor)
    }
}

#[derive(Debug)]
struct PopupLayout {
    x: usize,
    y: usize,
    width: usize,
    rows: usize,
    first: usize,
}

fn popup_line(
    out: &mut impl Write,
    x: usize,
    y: usize,
    text: &str,
    selected: bool,
) -> io::Result<()> {
    queue!(
        out,
        cursor::MoveTo(
            u16::try_from(x).unwrap_or(0),
            u16::try_from(y).unwrap_or(0)
        ),
        SetBackgroundColor(if selected {
            Color::DarkCyan
        } else {
            Color::DarkGrey
        }),
        SetForegroundColor(Color::White),
        Print(text)
    )
}

#[derive(Default)]
struct Pending {
    args: Option<Vec<Vec<u8>>>,
    confirmation: Option<u64>,
}

impl Pending {
    fn decide(&mut self, yes: bool) -> Option<Vec<Vec<u8>>> {
        self.confirmation = None;
        let args = self.args.take();
        if yes { args } else { None }
    }
}

pub struct Options {
    pub limits: Limits,
    pub reply_threshold: u64,
    pub yes: bool,
    pub theme: Theme,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ready,
    Busy,
    Stateful,
    Streaming,
    Disconnected,
}

struct App {
    editor: Editor,
    catalog: Catalog,
    workers: Workers,
    config: Config,
    client_id: Option<i64>,
    mode: Mode,
    output: VecDeque<String>,
    scroll: usize,
    status: String,
    popup: Option<Popup>,
    scan: Option<Discovery>,
    completion_span: Range<usize>,
    pending: Pending,
    options: Options,
}

pub fn run(session: Session, options: Options) -> io::Result<()> {
    let config = session.config.clone();
    let workers = Workers::start(session);
    let mut app = App {
        editor: Editor::default(), catalog: Catalog::bundled(), workers, config,
        client_id: None, mode: Mode::Busy,
        output: VecDeque::from(["rmycli — Tab: complete • F1: full help • Ctrl-D: exit • \\reconnect: reconnect".into()]),
        scroll: 0, status: "Loading server command metadata…".into(), popup: None,
        scan: None, completion_span: 0..0, pending: Pending::default(), options,
    };
    let _screen = Screen::enter()?;
    let mut dirty = true;
    loop {
        for _ in 0..64 {
            let Ok(event) = app.workers.events.try_recv() else {
                break;
            };
            app.on_worker(event);
            dirty = true;
        }
        if dirty {
            app.draw()?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                TerminalEvent::Key(key)
                    if key.kind != KeyEventKind::Release =>
                {
                    if app.on_key(key) {
                        return Ok(());
                    }
                }
                TerminalEvent::Paste(text) => {
                    app.invalidate();
                    // Pasting never submits commands, including embedded newlines.
                    app.editor.insert(&text.replace(['\r', '\n'], " "));
                }
                _ => {}
            }
            dirty = true;
        }
    }
}

impl App {
    fn log(&mut self, lines: impl IntoIterator<Item = String>) {
        self.output.extend(lines);
        while self.output.len() > 2000 {
            self.output.pop_front();
        }
        self.scroll = 0;
    }

    fn invalidate(&mut self) {
        self.workers.invalidate();
        self.popup = None;
        self.scan = None;
        self.pending = Pending::default();
        self.status.clear();
    }

    fn on_worker(&mut self, event: Event) {
        match event {
            Event::Connected {
                catalog,
                config,
                client_id,
            } => {
                self.log(catalog.notices.clone());
                self.status = format!(
                    "Connected • {} commands • database {}",
                    catalog.commands.len(),
                    config.info.redis.db
                );
                self.catalog = *catalog;
                self.config = config;
                self.client_id = client_id;
                self.mode = Mode::Ready;
            }
            Event::Executed {
                lines,
                config,
                allowed,
                streaming,
                client_id,
                connected,
            } => {
                self.log(lines);
                self.config = config;

                self.client_id = client_id;
                self.mode = if !connected {
                    Mode::Disconnected
                } else if streaming {
                    Mode::Streaming
                } else if allowed {
                    Mode::Ready
                } else {
                    Mode::Stateful
                };
                self.status = if connected {
                    if allowed {
                        String::new()
                    } else {
                        "Background assistance paused for session state".into()
                    }
                } else {
                    "Connection lost; command was not retried. Use \\reconnect."
                        .into()
                };
            }
            Event::Output(lines) => self.log(lines),
            Event::Failed(message) => {
                self.status = message;
                self.mode = Mode::Disconnected;
            }
            Event::Notice(message) => self.status = message,
            Event::Scanned { generation, state }
                if generation == self.workers.current() && !state.cancelled =>
            {
                self.scan_result(state);
            }
            Event::Estimated { generation, count }
                if generation == self.workers.current() =>
            {
                if estimate::needs_confirmation(
                    count,
                    self.options.reply_threshold,
                    self.options.yes,
                ) {
                    self.pending.confirmation = count;
                    self.status = format!(
                        "Continue? [y/N] May return about {} elements (not bytes; data can change).",
                        count.unwrap_or_default()
                    );
                } else if let Some(args) = self.pending.decide(true) {
                    self.execute(args);
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    self.workers.invalidate();
                    return true;
                }
                KeyCode::Char('c') => {
                    self.invalidate();
                    if matches!(self.mode, Mode::Busy | Mode::Streaming) {
                        if let Some(client_id) = self.client_id {
                            let _ = self.workers.assistance.send(
                                Assistance::Cancel {
                                    config: self.config.clone(),
                                    client_id,
                                },
                            );
                            self.status = "Closing the active connection; use \\reconnect afterward".into();
                        } else {
                            self.status =
                                "CLIENT ID unavailable; Ctrl-D exits".into();
                        }
                    } else {
                        self.editor.line.clear();
                        self.editor.cursor = 0;
                    }
                    return false;
                }
                KeyCode::Char('a') => {
                    self.invalidate();
                    self.editor.cursor = 0;
                    return false;
                }
                KeyCode::Char('e') => {
                    self.invalidate();
                    self.editor.cursor = self.editor.line.len();
                    return false;
                }
                KeyCode::Char('u') => {
                    self.invalidate();
                    self.editor.line.drain(..self.editor.cursor);
                    self.editor.cursor = 0;
                    return false;
                }
                KeyCode::Char('k') => {
                    self.invalidate();
                    self.editor.line.truncate(self.editor.cursor);
                    return false;
                }
                KeyCode::Char('l') => {
                    self.output.clear();
                    return false;
                }
                _ => return false,
            }
        }
        if self.confirm_key(key.code) {
            return false;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.complete(key.code == KeyCode::BackTab);
            }
            KeyCode::Enter => {
                if let Some(popup) = self.popup.take() {
                    popup.accept(&mut self.editor);
                    self.invalidate();
                } else if self.mode != Mode::Busy {
                    return self.submit();
                }
            }
            KeyCode::Esc => self.invalidate(),
            KeyCode::F(1) => {
                let ctx = input::context(&self.editor.line, self.editor.cursor);
                self.log(self.catalog.help(&ctx));
            }
            KeyCode::PageUp => {
                self.scroll = (self.scroll + 10).min(self.output.len());
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            code => {
                self.invalidate();
                self.edit(code);
            }
        }
        false
    }

    fn edit(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.editor.insert(&c.to_string()),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.cursor = 0,
            KeyCode::End => self.editor.cursor = self.editor.line.len(),
            KeyCode::Up => self.editor.history(true),
            KeyCode::Down => self.editor.history(false),
            KeyCode::Backspace if self.editor.cursor > 0 => {
                let end = self.editor.cursor;
                self.editor.left();
                self.editor.line.drain(self.editor.cursor..end);
            }
            KeyCode::Delete if self.editor.cursor < self.editor.line.len() => {
                self.editor.line.remove(self.editor.cursor);
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, code: KeyCode) -> bool {
        if self.pending.confirmation.is_some() {
            if matches!(code, KeyCode::Char('y' | 'Y')) {
                if let Some(args) = self.pending.decide(true) {
                    self.execute(args);
                }
            } else if matches!(
                code,
                KeyCode::Char('n' | 'N') | KeyCode::Enter | KeyCode::Esc
            ) {
                self.pending.decide(false);
                self.status = "Not executed".into();
            }
            return true;
        }
        if self.scan.is_some() {
            match code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(state) = self.scan.take() {
                        self.status = state.count_description();
                        self.show_candidates(
                            self.key_candidates(state.keys),
                            false,
                        );
                    }
                }
                KeyCode::Char('c' | 'C') => {
                    if let Some(state) = self.scan.take() {
                        if state.complete || state.hard_limit {
                            self.scan = Some(state);
                        } else {
                            self.request_scan(state);
                        }
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Enter | KeyCode::Esc => {
                    self.invalidate();
                }
                _ => {
                    self.invalidate();
                    return false;
                }
            }
            return true;
        }
        false
    }

    fn complete(&mut self, reverse: bool) {
        if let Some(popup) = &mut self.popup {
            popup.cycle(reverse);
            return;
        }
        self.invalidate();
        let ctx = input::context(&self.editor.line, self.editor.cursor);
        self.completion_span = ctx.replace.clone();
        let literals = self.catalog.literals(&ctx);
        if self
            .catalog
            .resolve(&ctx.args)
            .is_some_and(|(command, words)| {
                command.key_position(&ctx, words)
                    || (literals.is_empty() && command.needs_probe(&ctx, words))
            })
        {
            if self.mode == Mode::Ready {
                self.request_scan(Discovery::default());
            } else {
                self.status = "Key discovery paused while the session is busy or stateful".into();
            }
        } else {
            let candidates = literals.iter().map(|v| input::quote(v)).collect();
            self.show_candidates(candidates, true);
        }
    }

    fn request_scan(&mut self, state: Discovery) {
        let ctx = input::context(&self.editor.line, self.editor.cursor);
        let kind = self
            .catalog
            .resolve(&ctx.args)
            .and_then(|(command, _)| hints::key_type(&command.name));
        let probe =
            self.catalog
                .resolve(&ctx.args)
                .and_then(|(command, words)| {
                    (!command.key_position(&ctx, words)
                        && command.needs_probe(&ctx, words))
                    .then(|| (ctx.args.clone(), ctx.index))
                });
        self.status = "Discovering keys… Esc or editing cancels".into();
        let mut limits = self.options.limits.clone();
        // Each explicit continuation permits another threshold-sized page.
        limits.threshold = limits.threshold.saturating_add(state.keys.len());
        let _ = self.workers.assistance.send(Assistance::Scan {
            generation: self.workers.current(),
            config: self.config.clone(),
            prefix: ctx.prefix,
            kind,
            state,
            limits,
            probe,
        });
    }

    fn scan_result(&mut self, state: Discovery) {
        self.status = format!("{} {}", state.count_description(), state.notice);
        if state.complete && state.keys.len() <= self.options.limits.threshold {
            self.show_candidates(self.key_candidates(state.keys), true);
        } else {
            self.status.push_str(if state.complete || state.hard_limit {
                " • Show candidates? [y/N]"
            } else {
                " • [y] show available [c] continue scan [N] cancel"
            });
            self.scan = Some(state);
        }
    }

    fn key_candidates(
        &self,
        keys: std::collections::BTreeSet<Vec<u8>>,
    ) -> Vec<String> {
        let original = &self.editor.line[self.completion_span.clone()];
        keys.into_iter()
            .map(|key| input::quote_like(&key, original))
            .collect()
    }

    fn show_candidates(&mut self, candidates: Vec<String>, unique: bool) {
        match candidates.len() {
            0 => self.status.push_str(" • No candidates available"),
            1 if unique => {
                self.editor
                    .replace(self.completion_span.clone(), &candidates[0]);
            }
            _ => {
                self.popup = Some(Popup {
                    candidates,
                    selected: 0,
                    span: self.completion_span.clone(),
                });
            }
        }
    }

    fn submit(&mut self) -> bool {
        let parsed = input::parse(&self.editor.line);
        if let Some(error) = parsed.error {
            self.status = error;
            return false;
        }
        let mut args: Vec<_> =
            parsed.words.into_iter().map(|w| w.bytes).collect();
        if args.is_empty() {
            return false;
        }
        // Local backslash commands are editor controls, not Redis arguments.
        if let Some(local) = self
            .editor
            .line
            .split_whitespace()
            .next()
            .filter(|word| word.starts_with('\\'))
        {
            args[0] = local.as_bytes().to_vec();
        }
        if matches!(args[0].as_slice(), b"\\quit" | b"\\q")
            || (args.len() == 1
                && (args[0].eq_ignore_ascii_case(b"QUIT")
                    || args[0].eq_ignore_ascii_case(b"EXIT")))
        {
            return true;
        }
        if args.len() == 1 && args[0].eq_ignore_ascii_case(b"CLEAR") {
            self.invalidate();
            self.editor.remember();
            self.editor.line.clear();
            self.editor.cursor = 0;
            self.output.clear();
            self.scroll = 0;
            return false;
        }
        if self.mode == Mode::Streaming {
            self.status =
                "Ctrl-C closes the stream; then use \\reconnect".into();
            return false;
        }
        self.invalidate();
        if args[0] == b"\\reconnect" {
            self.mode = Mode::Busy;
            self.status = "Reconnecting…".into();
            self.editor.line.clear();
            self.editor.cursor = 0;
            let _ = self.workers.execution.send(Execution::Reconnect);
            return false;
        }
        if args[0].eq_ignore_ascii_case(b"HELP") || args[0] == b"\\help" {
            self.local_help(&args);
            return false;
        }
        if self.mode == Mode::Disconnected {
            self.status =
                "Disconnected; use \\reconnect. Commands are never replayed."
                    .into();
            return false;
        }
        self.pending.args = Some(args.clone());
        if self.mode == Mode::Ready
            && !self.options.yes
            && args[0].eq_ignore_ascii_case(b"LRANGE")
        {
            self.status = "Estimating reply size… Esc cancels".into();
            let _ = self.workers.assistance.send(Assistance::Estimate {
                generation: self.workers.current(),
                config: self.config.clone(),
                args,
            });
        } else {
            self.pending.args = None;
            self.execute(args);
        }
        false
    }

    fn execute(&mut self, args: Vec<Vec<u8>>) {
        self.editor.remember();
        self.log([format!(
            "{}{}",
            prompt(self.config.info.redis.db),
            self.editor.line
        )]);
        self.editor.line.clear();
        self.editor.cursor = 0;
        self.mode = Mode::Busy;
        self.status = "Executing… Ctrl-C closes this connection".into();
        let _ = self.workers.execution.send(Execution::Run(args));
    }

    fn local_help(&mut self, args: &[Vec<u8>]) {
        if args.len() == 1 {
            self.log(["HELP command: full command help; HELP @group: command list".into(),
                "Tab / Shift-Tab: cycle; Enter: accept candidate (press again to execute)".into(),
                "Esc: cancel completion; arrows / Ctrl-A/E/U/K: edit; Up/Down: history".into(),
                "F1: full current help; PageUp/PageDown: scroll; Ctrl-D or \\quit: exit".into(),
                "CLEAR or Ctrl-L: clear the transcript".into(),
                "\\reconnect: new connection and refreshed metadata; Ctrl-C: cancel input or close active connection".into()]);
        } else if args[1].starts_with(b"@") {
            let group = String::from_utf8_lossy(&args[1][1..]);
            let lines = self
                .catalog
                .commands
                .values()
                .filter(|c| c.docs["group"].as_str() == Some(group.as_ref()))
                .map(|c| format!("{} {}", c.name, c.grammar.syntax))
                .collect::<Vec<_>>();
            self.log(lines);
        } else {
            let line = args[1..]
                .iter()
                .map(|a| input::quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            self.log(self.catalog.help(&input::context(&line, line.len())));
        }
        self.editor.line.clear();
        self.editor.cursor = 0;
    }

    fn draw(&self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        if width < 4 || height < 4 {
            return Ok(());
        }
        let width = usize::from(width) - 1;
        let height = usize::from(height);
        let output_count = height.saturating_sub(3);
        let output: Vec<_> =
            self.output.iter().flat_map(|s| wrap(s, width)).collect();
        let end = output.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(output_count);
        let mut out = io::stdout();
        queue!(
            out,
            cursor::Hide,
            cursor::MoveTo(0, 0),
            Clear(ClearType::All)
        )?;
        let mut row = 0;
        for line in &output[start..end] {
            draw_line(&mut out, row, line, Color::White)?;
            row += 1;
        }
        row = output_count;
        let prompt = prompt(self.config.info.redis.db);
        let (line, column) = viewport(
            &format!("{prompt}{}", self.editor.line),
            prompt.len() + self.editor.cursor,
            width,
        );
        draw_line(&mut out, row, &line, Color::White)?;
        let input_row = row;
        let hint = self
            .catalog
            .inline_hint(&self.editor.line, self.editor.cursor);
        let used = display_width(&line);
        let hint = fit(&safe(&hint), width.saturating_sub(used));
        let args = input::parse(&self.editor.line)
            .words
            .into_iter()
            .map(|word| word.bytes)
            .collect::<Vec<_>>();
        let color = self
            .catalog
            .resolve(&args)
            .map_or(self.options.theme.inline_hint, |(command, _)| {
                self.options.theme.hint_color(&command.name)
            });
        queue!(out, SetForegroundColor(color), Print(hint), ResetColor)?;
        if let Some(popup) = &self.popup {
            let anchor = column.saturating_sub(display_width(&safe(
                &self.editor.line[popup.span.start..self.editor.cursor],
            )));
            popup.draw(&mut out, anchor, input_row, width)?;
        }
        row += 1;
        let status = if self.popup.is_some() {
            format!(
                "{} • Tab/Shift-Tab cycle • Enter accepts • Esc closes",
                self.status
            )
        } else {
            self.status.clone()
        };
        for line in wrap(&status, width).iter().take(height.saturating_sub(row))
        {
            draw_line(&mut out, row, line, Color::Green)?;
            row += 1;
        }
        queue!(
            out,
            ResetColor,
            cursor::MoveTo(
                u16::try_from(column).unwrap_or(0),
                u16::try_from(input_row).unwrap_or(0)
            ),
            cursor::Show
        )?;
        out.flush()
    }
}

fn prompt(database: i64) -> String {
    if database == 0 {
        "rmycli> ".into()
    } else {
        format!("rmycli[{database}]> ")
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Clip without wrapping or splitting a Unicode character.
fn fit(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

fn draw_line(
    out: &mut impl Write,
    row: usize,
    line: &str,
    color: Color,
) -> io::Result<()> {
    queue!(
        out,
        cursor::MoveTo(0, u16::try_from(row).unwrap_or(0)),
        SetForegroundColor(color),
        Print(line)
    )
}

fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn truncate(text: &str, width: usize) -> String {
    wrap(text, width).into_iter().next().unwrap_or_default()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut result = vec![String::new()];
    let mut used = 0;
    for c in safe(text).chars() {
        let size = c.width().unwrap_or(0);
        if used + size > width && used > 0 {
            result.push(String::new());
            used = 0;
        }
        if let Some(line) = result.last_mut() {
            line.push(c);
        }
        used += size;
    }
    result
}

fn viewport(line: &str, cursor: usize, width: usize) -> (String, usize) {
    let mut start = 0;
    let mut column: usize = display_width(&safe(&line[..cursor]));
    for (index, c) in line[..cursor].char_indices() {
        if column < width {
            break;
        }
        column = column.saturating_sub(display_width(&safe(&c.to_string())));
        start = index + c.len_utf8();
    }
    (truncate(&line[start..], width), column)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_database_prompt_and_unicode_clipping() {
        assert_eq!(prompt(0), "rmycli> ");
        assert_eq!(prompt(12), "rmycli[12]> ");
        assert_eq!(fit("你好abcdef", 5), "你好a");
        assert_eq!(fit("key start stop", 0), "");
    }

    #[test]
    fn popup_overlays_above_input_and_scrolls_with_selection() {
        let popup = Popup {
            candidates: (0..30).map(|n| format!("列表:{n}")).collect(),
            selected: 29,
            span: 0..0,
        };
        for (anchor, row, width) in [(12, 20, 80), (19, 5, 20), (2, 2, 4)] {
            let area = popup.layout(anchor, row, width);
            assert!(area.x + area.width <= width);
            assert!(area.y + area.rows + 2 <= row);
            if area.rows > 0 {
                assert!(area.first <= popup.selected);
                assert!(popup.selected < area.first + area.rows);
            }
        }
        let area = popup.layout(12, 20, 80);
        assert_eq!(area.x, 12);
        assert_eq!(area.rows, 8);
        let mut rendered = Vec::new();
        popup.draw(&mut rendered, 12, 20, 80).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("┌ 30/30 "));
        assert!(rendered.contains("列表:29"));
    }
    #[test]
    fn accepting_popup_only_changes_input_and_preserves_suffix() {
        let mut editor = Editor {
            line: "LRANGE li 0 -1".into(),
            cursor: 9,
            ..Editor::default()
        };
        let mut popup = Popup {
            candidates: vec!["list:a".into(), "list:b".into()],
            selected: 0,
            span: 7..9,
        };
        popup.cycle(true);
        popup.accept(&mut editor);
        assert_eq!(editor.line, "LRANGE list:b 0 -1");
        assert_eq!(editor.cursor, 13);
    }
    #[test]
    fn declining_confirmation_never_returns_a_command() {
        let mut pending = Pending {
            args: Some(vec![b"LRANGE".to_vec()]),
            confirmation: Some(1_000_000),
        };
        assert!(pending.decide(false).is_none());
        assert!(pending.args.is_none());
        assert!(pending.decide(true).is_none());
    }
    #[test]
    fn utf8_editing_viewport_and_terminal_escape_safety() {
        let mut editor = Editor::default();
        editor.insert("你好 key");
        editor.left();
        editor.insert("é");
        assert_eq!(editor.line, "你好 keéy");
        let (line, column) = viewport("你好abcdef", 12, 6);
        assert!(column < 6);
        assert_ne!(line, "");
        assert!(!safe("a\x1b[2J").contains('\x1b'));
    }

    fn test_app() -> (App, std::sync::mpsc::Receiver<Execution>) {
        use redis::IntoConnectionInfo;
        use std::sync::{Arc, atomic::AtomicU64, mpsc};
        let (execution, executed) = mpsc::channel();
        let (assistance, _assisted) = mpsc::channel();
        let (_events, events) = mpsc::channel();
        let app = App {
            editor: Editor {
                line: "GET a".into(),
                cursor: 5,
                ..Editor::default()
            },
            catalog: Catalog::bundled(),
            workers: Workers {
                execution,
                assistance,
                events,
                generation: Arc::new(AtomicU64::new(0)),
            },
            config: Config {
                info: ("localhost", 6379).into_connection_info().unwrap(),
                timeout: Duration::from_secs(1),
            },
            client_id: None,
            mode: Mode::Ready,
            output: VecDeque::new(),
            scroll: 0,
            status: String::new(),
            popup: None,
            scan: None,
            completion_span: 4..5,
            pending: Pending {
                args: Some(vec![b"GET".to_vec(), b"a".to_vec()]),
                confirmation: None,
            },
            options: Options {
                limits: Limits::default(),
                reply_threshold: 100,
                yes: false,
                theme: Theme::default(),
            },
        };
        (app, executed)
    }

    #[test]
    fn clear_is_local_and_preserves_session_and_history() {
        for mode in [
            Mode::Ready,
            Mode::Stateful,
            Mode::Disconnected,
            Mode::Streaming,
        ] {
            for line in ["clear", "CLEAR", "  ClEaR  ", "\"clear\""] {
                let (mut app, executed) = test_app();
                app.mode = mode;
                app.editor.line = line.into();
                app.editor.cursor = line.len();
                app.editor.history.push("PING".into());
                app.output.push_back("PONG".into());
                app.scroll = 1;
                app.status = "Old status".into();
                app.config.info.redis.db = 2;

                assert!(
                    !app.on_key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE
                    ))
                );

                assert!(app.output.is_empty());
                assert_eq!(app.scroll, 0);
                assert_eq!(app.editor.line, "");
                assert_eq!(app.editor.cursor, 0);
                assert_eq!(app.editor.history, ["PING", line]);
                assert_eq!(app.status, "");
                assert!(app.pending.args.is_none());
                assert!(app.mode == mode);
                assert_eq!(app.config.info.redis.db, 2);
                assert!(executed.try_recv().is_err());
            }
        }
    }

    #[test]
    fn clear_with_arguments_is_sent_to_redis() {
        let (mut app, executed) = test_app();
        app.editor.line = "clear key".into();
        app.editor.cursor = app.editor.line.len();
        app.output.push_back("PONG".into());

        assert!(!app.submit());

        assert_eq!(app.output.front().unwrap(), "PONG");
        let Execution::Run(args) = executed.try_recv().unwrap() else {
            panic!("expected a Redis command");
        };
        assert_eq!(args, [b"clear".to_vec(), b"key".to_vec()]);
    }

    #[test]
    fn editing_invalidates_scan_results_and_pending_execution() {
        let (mut app, executed) = test_app();
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.on_worker(Event::Scanned {
            generation: 0,
            state: Discovery {
                keys: std::collections::BTreeSet::from([b"a-key".to_vec()]),
                complete: true,
                ..Discovery::default()
            },
        });
        app.on_worker(Event::Estimated {
            generation: 0,
            count: Some(1),
        });
        assert_eq!(app.editor.line, "GET ab");
        assert!(app.popup.is_none());
        assert!(app.pending.args.is_none());
        assert!(executed.try_recv().is_err());
    }
}
