# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Complete Redis parameter names with Tab in `CONFIG GET` and `CONFIG SET`.
  Both accept prefixes and Redis globs for lookup, including `slowlog-*`.
- Keep reply-size hints current while you pause on a line. Cheap counts such
  as `SCARD` and `DBSIZE` refresh every 500 ms by default
  (`size-hint-refresh-ms`), and `KEYS <pattern>` scans every 5000 ms
  (`scan-hint-refresh-ms`). Each interval starts when the previous probe
  finishes, so refreshes never pile up. The current hint is replaced only
  when the new value is ready, without repeating the running count. Set
  either value to zero or less to turn that refresh off.

### Fixed

- Fix the macOS distribution workflow's `lipo` argument order so it can
  verify both architectures before signing and packaging the universal binary.

## [0.2.0] - 2026-11-17

### Added

- See live reply-size hints for `GET`, `LRANGE`, hash reads, and set and sorted
  set operations. Counts show their units and multi-key bounds use `≤`.
  Background probes pause during transactions, WATCH, and streaming. Configure
  the colors and warning threshold (default 10,000), or disable probes with
  `size-hints = false`.
- Preview a `KEYS` pattern while typing. `KEYS *` uses one `DBSIZE` request.
  Other patterns show a running estimate from `SCAN MATCH` alongside the
  initial database size. Counts can include duplicates. Editing or submitting
  cancels the scan. Configure the batch size and delay under `[keys-hints]`.
- Press Ctrl+X followed by Enter to stream `KEYS <pattern>` using `SCAN`,
  including inside tmux. Set `alternate-prefix` to change the shortcut.
  Ctrl+Enter remains available on terminals that support it.
- Recall the last 500 history entries across sessions. Recall and Ctrl-R search
  use the current endpoint by default. Set `history-per-server = false` to
  search all endpoints. Ctrl-R supports fuzzy and exact matches. Enter recalls
  a command for editing, and Esc or Ctrl-C restores the draft.
- Save multiple ACL accounts per endpoint in a separate credential store.
  Automatic saving is off by default. Set `save-auth` to `"local"` or
  `"always"` to save credentials after successful authentication, including
  on redirect destinations. Select an account with `--user`, or set its
  default offline with `--set-default-user`. Without either, the first
  username in alphabetical order is used.
- Encrypt saved credentials with a master passphrase using Argon2id and
  XChaCha20-Poly1305. Set `encrypt-auth` for new saves. To migrate existing
  credentials, run `rmycli auth encrypt`, verify the encrypted store, then
  remove the retained plaintext file. Use `auth passwd` to change the
  passphrase and interactive `\auth` commands to lock or unlock the store.
- Complete shell options, configured instances, and locally saved usernames in
  Bash, Zsh, Fish, PowerShell, and Elvish. Completion runs offline and leaves
  encrypted credential stores locked.
- Set the default RESP version globally or per instance with `resp = 2` or
  `resp = 3`. Command-line `-2` and `-3` take precedence. Users of development
  checkouts with the temporary `resp3` boolean should replace it with `resp`.
- Follow standalone Valkey primary redirects, including during `--scan`.
  Capability detection retries after authentication. Redirects are bounded
  and skip watched or transactional commands.
- GitHub Actions runs Linux and macOS CI and builds checksummed static Linux
  x86-64 and ARM64 archives and a universal macOS archive. Contributors can
  test a distribution binary with `RMYCLI_TEST_BINARY` and prepare a tagged
  release with the repository release skill.

### Changed

- The default prompt shows the session user, endpoint, and nonzero database
  with a colored `❯`. Customize `prompt` with connection fields and named,
  RGB, or indexed colors. The sample config includes an ASCII template.
- Ctrl-C at the idle prompt exits. During command execution or MONITOR it
  closes the socket locally. The shell reconnects quietly after cancellation
  or connection loss. A new command or `\reconnect` retries a failed
  connection. Reconnection starts a fresh session without replaying commands
  or restoring transactions, watches, or subscriptions.
- Enter commands and add subscriptions while subscribed. Redis enforces the
  protocol's command restrictions. Ctrl-C at the subscribed prompt removes
  ordinary channel subscriptions while keeping the connection, pattern
  subscriptions, and shard subscriptions active.
- Detect cluster mode on connection and follow MOVED/ASK automatically.
  Set `auto-cluster = false` to disable detection. The explicit `-c` switch
  still enables redirect following.
- Display queued SCAN, Pub/Sub, and MONITOR output as it arrives. MONITOR
  preserves server quoting and hides the prompt until the stream ends.
- `--askpass` supports paste, limits input to 1,024 bytes, and clears the
  entered text from its buffer after use.
- Source builds now require a C compiler and make for bundled libsodium.

### Fixed

- Drain all subscribe and unsubscribe acknowledgments, including RESP3 pushes
  and multi-channel replies. Subsequent commands receive their own replies,
  even when push display is disabled.
- Quote bulk strings in interactive and `--no-raw` output. Format RESP3 maps
  as numbered key/value pairs, mark set entries with `~`, label empty hashes
  and sets, and align nested replies under multi-digit indexes.
- Honor Shift and Caps Lock on Kitty keyboard protocol terminals. Decode
  command names starting with a hex escape and reject malformed escapes such
  as `\x+1`.
- Follow repeated tokens in live `COMMAND DOCS`, retain hints for exclusive
  score bounds, and accept mixed-case group names in `HELP @group`.
- Keep WATCH active after a failed EXEC outside MULTI and ignore stray QUEUED
  replies outside transactions.
- Explain when authentication is required and refresh completion and help after
  AUTH or HELLO AUTH without disturbing MULTI or WATCH. HELLO now parses
  passwords and client names correctly when they resemble option names.
- Skip unreadable or unsupported system certificate entries. Connections
  using `--insecure` can start without a system certificate store.
- Merge concurrent history saves and write history and plaintext credentials
  atomically with owner-only permissions on Unix. Busy-file errors identify
  the file and lock. Encrypted stores reject stale writes.
- Terminal smoke tests wait for the post-authentication metadata refresh and
  accept the server's argument name for `XGROUP DESTROY` across Redis versions.

### Security

- Exclude credential-bearing Redis URLs from saved history and sensitive
  commands from history recall and submitted-command transcripts.

## [0.1.0] - 2026-11-04

### Fixed

- Avoid flashing the startup metadata-loading message on fast connections;
  show it only after 200 ms or when you type.
- Display RESP3 invalidations while the interactive prompt is idle, without
  needing another command. Async reads preserve partial messages and respect
  `--show-pushes` and session state. Push output is bold and preserves the
  current input and cursor.
- Buffer terminal redraws and synchronize updates so submitting a command does
  not briefly show an intermediate frame with stale inline hints.
- Stop flashing a grey `Executing…` status on a fresh prompt after every
  command. The prompt now waits below the submitted command, and the busy
  status appears only when a reply takes longer than 200 ms or you type.

### Changed

- Use Ctrl+Enter instead of Shift+Enter for alternate KEYS scanning. Request
  modifier press/release events and show `[Ctrl]` in the prompt while either
  Ctrl key is held on terminals that support the Kitty keyboard protocol.
- Upgrade redis-rs to 1.8.1 and replace synchronous session polling with async
  TCP/Unix/TLS reads, retaining the same connection for streaming and stateful
  commands. Remove the vendored Redis crate and its idle-read patch.

- Show server error messages directly, such as `(error) syntax error`, without
  the Redis library's preamble; retain context for client and connection errors.
- Render interactive invalidation pushes as bold `-> invalidate: 'key'` lines,
  one per key, or `-> invalidate: (nil)` for cache-wide invalidation.
- Match redis-cli's raw-output default when stdout is redirected; `--no-raw`
  retains human-readable output. Raw integers, maps, nils, and delimiters now
  work consistently for scripting.

- Bound command history with a ring buffer and avoid reparsing the input line
  twice when computing completion context.

- Use the normal terminal buffer, starting the prompt at the current cursor row
  and advancing with replies. Output remains in terminal scrollback after exit.
- Remove the startup shortcut banner and persistent connection footer.
  F1 opens a dismissible, scrollable panel with current command help, shortcuts,
  and connection details; `HELP` opens editor help. PageUp/PageDown scroll help,
  while earlier output is accessible through terminal scrollback.
- Show remaining command arguments as grey inline hints instead of a help
  block below the input. Detailed documentation remains available with F1
  and `HELP`.
- Render completion choices in a bordered, highlighted popup below the current
  argument, reserving space as needed, with scrolling and terminal-edge clipping.
- Omit the database suffix from the prompt and transcript for database zero.

### Added

- Link the GitHub repository as both the package homepage and source repository.
- License the application source code under MIT, retaining the bundled Redis
  documentation's separate CC BY-SA 4.0 license and attribution.
- Add crates.io discovery metadata, a verified Rust 1.88 minimum, an explicit
  package file list that excludes local recordings, and installation and
  first-release publishing instructions.
- Experimental Ctrl+Enter on `KEYS <pattern>` streams matching keys through
  SCAN. Ctrl-C stops iterations without exiting or disconnecting; normal Enter
  retains ordinary KEYS behavior. Requires a terminal that reports Ctrl+Enter.
- Redis-cli-compatible RESP2/RESP3 selection, password aliases and hidden
  password prompting, connection timeout in seconds, client names, and bounded
  MOVED/ASK redirect following outside transactions and WATCH.
- TLS CA files/directories, client certificates and keys, and explicit
  `--insecure`, including support alongside connection URLs and saved instances.
- Binary stdin arguments (`-x`/`-X`), quoted input, command repetition and delays,
  Lua files with key/argument separation, and streaming SCAN with glob filters.
- CSV, JSON, ASCII-safe JSON, raw reply delimiters, RESP3 push display controls,
  verbose connection status, and familiar version/error-status switches.
- Grouped `--help` with practical examples and documented compatibility limits;
  isolated CLI, TLS, and redirect regression tests.

- Interactive `CLEAR` command (case insensitive) to clear the screen
  locally, like Ctrl-L, without changing the Redis session.
- Commented `rmycli.dist.toml` configuration reference covering all supported
  settings, defaults, color formats, and named-instance authentication.
- Local `config get`, `config set`, and `config unset` commands with effective
  defaults, typed validation, comment-preserving edits, and atomic file updates.
  Color settings include terminal previews with `--color` and `NO_COLOR` controls.
- TOML configuration from XDG directories (normally `~/.config/rmycli.toml`),
  explicit `--config` and `--no-config` switches, and startup validation.
- Configurable inline-hint colors, including per-command and subcommand
  overrides, named colors, RGB, and indexed terminal colors.
- Named connection instances selected with `--instance`, including host, port,
  database, TLS, ACL user, and passwords from the file or environment. Explicit
  connection flags and `REDISCLI_AUTH` override instance settings.
- Working Redis shell with TCP, Unix socket, authenticated and verified TLS
  connections; command-line and stdin execution; and binary-safe reply output.
- Live syntax, summary, argument, version, group, and complexity help from
  `COMMAND DOCS`, with the full Redis documentation catalog bundled as a
  labeled fallback. Command and subcommand completion uses the live catalog.
- Metadata-driven key argument completion, separate Redis value-type hints,
  and a cancellable SCAN worker with literal prefix matching, deduplication,
  configurable discovery budgets, partial-result notices, and display prompts.
- Completion popup with Tab/Shift-Tab cycling and separate acceptance and
  execution; cursor editing, in-memory history, paste handling, and full help.
- `LRANGE` element-count estimates and confirmation, including a defined
  noninteractive refusal policy and explicit `--yes` override.
- Separate auxiliary connections, database and transaction state tracking,
  explicit reconnect with refreshed metadata, and connection cancellation.
- Pub/Sub and MONITOR reply display; stop interactive streams with Ctrl-C
  and reconnect before sending further commands.
- Isolated Redis integration fixtures and real-terminal smoke tests covering
  execution, completion, reply confirmation, cancellation, and streaming.
- Initial project README describing the Redis client vision, metadata-based
  completion, bounded key discovery, and planned reply-size guidance.
- Shared contributor instructions in `AGENTS.md`, with `CLAUDE.md` linking
  to the same file.
- Standard Rust formatting configuration with an 80-column target.
- Clippy `all`, `nursery`, and `pedantic` warnings at the binary crate root,
  with formatting, strict linting, and tests required before every commit.
