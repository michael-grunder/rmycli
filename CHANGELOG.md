# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Configurable live reply-size hints: `size-hints` (default enabled),
  `theme.size-hint` (default `dark_cyan`), `size-hint-threshold` (default 10,000),
  and `theme.size-hint-warning` (default `red`). Counts at or above the threshold
  use the warning color. Disabling hints also stops their background probes.
  All four settings support `config get/set/unset`.

- Live reply-size hints while typing, such as `SMEMBERS s [3 members]` or
  `SUNION a b [≤5 members]`. Keys come from the server's `COMMAND` key specs;
  `SCARD`/`ZCARD`/`HLEN`/`LLEN`/`STRLEN` probes run on the background
  connection and pause during `MULTI`, `WATCH`, and streaming. Supported:
  `SMEMBERS`, `SUNION`, `SINTER`, `SDIFF`, `ZUNION`, `ZINTER`, `ZDIFF`,
  `HGETALL`, `HKEYS`, `HVALS`, `LRANGE`, and `GET`.

- Global numeric `resp` configuration (`2` or `3`, default `2`) with per-instance
  overrides, replacing the unreleased `resp3` boolean. Instances inherit
  the global default when omitted; `-2` and `-3` override configuration.
  Read, set, or restore inheritance with `config get/set/unset`.

- Dynamic shell completions for Bash, Zsh, Fish, PowerShell, and Elvish using
  clap's command-line definitions. Complete configured instance names and local
  saved usernames for the selected endpoint, including partial names. Completion
  never connects or prompts; encrypted credential stores remain locked.

- Optional encrypted credential storage using libsodium Argon2id and
  XChaCha20-Poly1305. Enable `encrypt-auth` for new saves, or migrate with
  `rmycli auth encrypt`, which retains the original plaintext file and prints
  its path. Encrypted files are automatically detected. Hidden master-passphrase
  prompts, `auth passwd`, and interactive `\auth lock|unlock|passwd` manage the
  store. Private atomic writes reject stale updates from concurrent sessions.

- Configurable interactive prompts with named connection placeholders, literal
  brace escaping, and named, RGB, or indexed foreground colors. The default
  uses cyan connection details and a green Unicode `❯`. Manage the template
  with `config get/set/unset prompt`; the example config documents all fields,
  color formats, limits, and copyable recipes, including the older ASCII prompt.

### Fixed

- Ctrl-C at an idle prompt now exits like `redis-cli` instead of clearing
  the input. It still stops scans, unsubscribes while subscribed, and cancels
  executing commands without exiting.
- Render RESP3 maps as numbered `key => value` pairs, distinguish set entries
  with `~`, and align collection indexes and nested replies.

- Show MONITOR lines with Redis's quoting and hide the prompt while monitoring.
  Ctrl-C closes the socket locally before reconnecting, avoiding cancellation
  handshake traffic in the monitor output and removing the need for CLIENT KILL
  permissions.

- Always double-quote bulk string replies in interactive and `--no-raw`
  output, including strings inside collections, like redis-cli.
- Accept interactive commands and additional subscriptions while subscribed,
  leaving command restrictions to Redis. Ctrl-C sends argument-free `UNSUBSCRIBE`
  without disconnecting; pattern and shard subscriptions remain active. Drain
  every subscription acknowledgment and keep published messages separate from
  command replies in RESP2 and RESP3, including when push display is disabled.
- Type shifted characters correctly on Kitty keyboard protocol terminals, where
  Shift+a produced `a` and Shift+; produced `;`. Caps Lock now applies as well.
- Stop hanging on RESP3 `UNSUBSCRIBE`, `PUNSUBSCRIBE`, and `SUNSUBSCRIBE`, whose
  acknowledgments are pushes, and read one acknowledgment per named channel so
  later commands no longer receive the previous command's reply.
- Recognize the `multiple_token` flag from live `COMMAND DOCS`, so hints and
  completion follow repeated tokens such as `SORT key GET a GET b` on real
  servers instead of only with bundled metadata.
- Keep tracking `WATCH` after a failed `EXEC` without `MULTI`, which leaves the
  server's watch in place, and ignore `QUEUED` replies outside a transaction.
- Send commands whose first word starts with a `\xHH` escape as decoded bytes
  rather than as a literal backslash command name.
- Reject `\x+1` and similar sign-prefixed input instead of decoding it as hex.
- Show `(empty hash)` and `(empty set)` for empty RESP3 replies. Empty maps
  previously printed nothing. Align nested replies under indexes of 10 or more.
- Keep inline hints for exclusive score bounds such as `ZCOUNT key (1 5`.
- Accept any case in `HELP @group`.
- Tolerate unreadable or unsupported entries in the system certificate store,
  and do not require system certificates with `--insecure`.

- Exclude credential-bearing Redis URLs from saved history, and omit sensitive
  commands from recalled history and submitted-command transcripts.

- Synchronize the terminal authentication test with the metadata refresh so
  release builds do not race completion against the catalog update.

- Explain when a server requires authentication, and reload completion and help
  metadata after successful AUTH or HELLO AUTH without disturbing MULTI/WATCH.
- Parse HELLO options without mistaking passwords or client names for options.
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

- History and plaintext credential saves share one locked, atomic write path.
  A busy-file error now names the file as well as its stale lock.

- Reconnect interactive sessions quietly after cancellation or connection loss.
  Failed reconnects can be retried by submitting a new command or `\reconnect`.
  Interrupted commands are never replayed; connection-local state such as
  transactions, WATCH, and subscriptions is not restored.

- Render queued streaming output without waiting for the input poll interval.
  An interactive `KEYS` scan of 300,000 keys dropped from about 95 seconds to
  about 1 second, and MONITOR and Pub/Sub output is no longer limited to 64
  messages per 40 ms poll.
- `--askpass` shares the hidden prompt used for master passphrases, adding
  paste support, a 1024-byte limit, and zeroizing of the entered text.
- Automatically select the first saved account in alphabetical username order
  when no user or saved default is specified. Explicit users and saved defaults
  retain precedence.
- Include the session username in the prompt and command transcript, such as
  `alice@127.0.0.1:6379>`, using `default` when no username is supplied.

- Save credentials for successfully authenticated MOVED, ASK, and Valkey REDIRECT
  destinations when permitted by `save-auth`, so later direct connections to
  those nodes can authenticate automatically.

- Automatically detect cluster mode after connecting; `auto-cluster = false`
  disables automatic MOVED/ASK following, while `-c` remains an explicit override.

- Show the current host and port (or Unix socket path) in the interactive
  prompt and command transcript, like redis-cli, retaining nonzero database
  suffixes and the Ctrl indicator.

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

- Keep multiple saved ACL accounts per server, preserving existing credentials.
  Accounts can be selected with `--user` or a saved default.
  `--set-default-user USER` selects an existing saved account without
  connecting, including `default` for password-only authentication.
- `history-per-server` setting (default: `true`); set it to `false` for flat
  history recall and search across servers while retaining stored server tags.
- Persistent interactive history in `~/.config/rmycli_history` (or the XDG
  configuration directory), shared in one file capped at 500 entries across
  servers. Recall and search load only the connected endpoint's history;
  sessions merge new entries on exit using atomic, owner-only writes on Unix.
  Common credential-bearing commands are excluded from disk history.
- Ctrl-R history search with fuzzy terms, apostrophe-prefixed exact substrings,
  smart case, and newest-first deduplicated matches. Enter recalls without
  executing; Esc/Ctrl-C restores the draft and cursor. Includes plain terminal
  and isolated tmux coverage without requiring extended keyboard support.
- GitHub Actions checks for formatting, Clippy, and the existing Rust/Redis
  tests on Linux and macOS, plus Linux terminal smoke tests.
- Manual and version-tag distribution builds for static musl Linux x86-64
  and ARM64 binaries and a universal macOS binary, with release-build tests,
  architecture/linkage checks, packaged documentation, and SHA-256 checksums.
- `RMYCLI_TEST_BINARY` selects a binary for the terminal smoke suite, allowing
  the same tests to exercise distribution builds.

- Detect Valkey and negotiate `CLIENT CAPA REDIRECT` to follow standalone
  primary redirects, including during `--scan`. Preserve connection settings,
  cap redirect chains, and avoid replaying transactional or watched commands.
  Retry detection after successful authentication without disturbing session
  state; unsupported or denied probes leave manual commands available.
- Separate `rmycli-auth.toml` server credentials, with automatic matching by
  endpoint and opt-in `save-auth = false`, `"local"`, or `"always"` in the main
  config. Successful authentication can save credentials atomically with
  owner-only permissions on Unix; automatic saving defaults to off.
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
