#!/usr/bin/env python3
"""Exercise the real terminal and binary against an isolated Redis server.

Run after cargo build. Requires Python 3, redis-server, redis-cli, and a Unix PTY.
No third-party Python dependencies or existing Redis instance are needed.
"""
import fcntl
import errno
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import socket as sockets
import struct
import subprocess
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/rmycli"


class Frame:
    """Read the cursor moves, clears, and colors emitted by our renderer."""
    def __init__(self, data, height=35, start_row=7):
        end = data.rfind(b"\x1b[?25h")
        self.cells = {(start_row - 1, x): c for x, c in enumerate("previous shell output")}
        self.colors = {}
        self.x, self.y = 0, start_row
        self.scrolled = []
        color = ""
        text = data[:end + 6 if end >= 0 else len(data)].decode("utf-8", errors="replace")
        for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", text):
            if part.startswith("\x1b["):
                if part.endswith("H"):
                    row, column = (int(n) for n in part[2:-1].split(";"))
                    self.y, self.x = row - 1, column - 1
                elif part.endswith("m"):
                    color = part[2:-1]
                elif part in ("\x1b[2J", "\x1b[J", "\x1b[0J"):
                    for cell in list(self.cells):
                        if part == "\x1b[2J" or cell >= (self.y, self.x):
                            self.cells.pop(cell, None)
                            self.colors.pop(cell, None)
            else:
                for char in part:
                    if char == "\r":
                        self.x = 0
                    elif char == "\n":
                        if self.y >= height - 1:
                            self.scrolled.append(self.row(0))
                            self.cells = {(y - 1, x): c for (y, x), c in self.cells.items() if y > 0}
                            self.colors = {(y - 1, x): c for (y, x), c in self.colors.items() if y > 0}
                        else:
                            self.y += 1
                    else:
                        self.cells[self.y, self.x] = char
                        self.colors[self.y, self.x] = color
                        self.x += 1

    def row(self, row):
        width = max((x + 1 for y, x in self.cells if y == row), default=0)
        return "".join(self.cells.get((row, x), " ") for x in range(width)).rstrip()

    def hint_is_inline(self, typed, hint, colors=("90", "38;5;8")):
        prompt = "rmycli> "
        return (self.row(self.y) == prompt + typed + hint
                and self.x == len(prompt + typed)
                and (not hint or self.colors.get((self.y, self.x)) in colors))


class Terminal:
    def __init__(self, socket, *options, env=None, start_row=7):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))
        connection = ["-s", socket] if socket else []
        config = [] if "--config" in options else ["--no-config"]
        self.process = subprocess.Popen(
            [str(BINARY), *connection, *config, *options], stdin=slave, stdout=slave,
            stderr=slave, env={**{k: v for k, v in os.environ.items()
                                 if k not in ("NO_COLOR", "REDIS_URL", "REDISCLI_AUTH")},
                 "TERM": "xterm-256color", **(env or {})},
        )
        os.close(slave)
        self.data = b""
        self.screen_data = b""
        self.height = 35
        self.start_row = start_row
        self.expect_frame(lambda f: f.row(f.y) == "rmycli>"
                          and not f.row(f.y + 1), "quiet connected prompt")
        assert b"\x1b[?1049h" not in self.screen_data
        assert b"Connected" not in self.screen_data
        assert b"Tab: complete" not in self.screen_data
        frame = Frame(self.screen_data, self.height, self.start_row)
        assert frame.y == start_row
        assert frame.row(start_row - 1) == "previous shell output"

    def read(self):
        chunk = os.read(self.master, 65536)
        queries = self.screen_data.count(b"\x1b[6n")
        self.data += chunk
        self.screen_data += chunk
        if self.screen_data.count(b"\x1b[6n") > queries:
            frame = Frame(self.screen_data, self.height, self.start_row)
            row = min(frame.y, self.height - 1)
            os.write(self.master, f"\x1b[{row + 1};{frame.x + 1}R".encode())

    def resize(self, height, width):
        self.data = b""
        self.height = height
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        os.kill(self.process.pid, signal.SIGWINCH)

    def send(self, data):
        self.data = b""
        os.write(self.master, data)

    def expect(self, text, timeout=5):
        deadline = time.monotonic() + timeout
        needle = text.encode()
        while needle not in self.data:
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"Missing {text!r}: {self.data[-6000:]!r}"
            ready, _, _ = select.select([self.master], [], [], remaining)
            if ready:
                self.read()
        return self.data

    def expect_frame(self, predicate, description, timeout=5):
        deadline = time.monotonic() + timeout
        while True:
            frame = Frame(self.screen_data, self.height, self.start_row)
            if b"\x1b[?25h" in self.data and predicate(frame):
                return frame
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"Missing {description}: {self.data[-6000:]!r}"
            ready, _, _ = select.select([self.master], [], [], remaining)
            if ready:
                self.read()

    def close(self):
        if self.process.poll() is None:
            os.write(self.master, b"\x04")
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        while select.select([self.master], [], [], 0)[0]:
            try:
                self.read()
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
        os.close(self.master)
        assert self.process.returncode == 0


def config_terminal_output(path, *args, no_color=None):
    """Run a local settings command with terminal stdout and capture its ANSI."""
    master, slave = pty.openpty()
    env = {k: v for k, v in os.environ.items()
           if k not in ("NO_COLOR", "REDISCLI_AUTH")}
    env.update(TERM="xterm-256color", REDIS_URL="invalid-url-not-used-by-config")
    if no_color is not None:
        env["NO_COLOR"] = no_color
    try:
        result = subprocess.run(
            [str(BINARY), "config", "--config", str(path), *args],
            stdin=subprocess.DEVNULL, stdout=slave, stderr=subprocess.PIPE,
            env=env, timeout=5,
        )
        assert result.returncode == 0, result.stderr
    finally:
        os.close(slave)
    data = b""
    try:
        while True:
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
            if not chunk:
                break
            data += chunk
    finally:
        os.close(master)
    return data


def main():
    with tempfile.TemporaryDirectory(prefix="rmycli-terminal-") as directory:
        settings_path = Path(directory) / "preview.toml"
        preview = config_terminal_output(settings_path, "set", "theme.inline-hint", "#12abef")
        assert b"#12abef  \x1b[38;2;18;171;239msample text\x1b[0m" in preview
        preview = config_terminal_output(settings_path, "get", "theme.inline-hint")
        assert b"\x1b[38;2;18;171;239msample text\x1b[0m" in preview
        plain = config_terminal_output(settings_path, "get", "theme.inline-hint", no_color="1")
        assert plain == b"#12abef\r\n"
        plain = config_terminal_output(settings_path, "get", "theme.inline-hint", "--color", "never")
        assert plain == b"#12abef\r\n"
        socket = str(Path(directory) / "redis.sock")
        server = subprocess.Popen(
            ["redis-server", "--port", "0", "--save", "", "--appendonly", "no",
             "--unixsocket", socket, "--dir", directory],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        terminals = []

        def redis(*args):
            return json.loads(subprocess.check_output(
                ["redis-cli", "-s", socket, "--json", *args], text=True,
            ))

        def cli(*args, stdin=None):
            return subprocess.run([str(BINARY), "--no-config", "-s", socket, *args],
                                  input=stdin, capture_output=True, timeout=5,
                                  env={k: v for k, v in os.environ.items()
                                       if k not in ("REDIS_URL", "REDISCLI_AUTH")})

        try:
            deadline = time.monotonic() + 5
            while not Path(socket).exists():
                assert time.monotonic() < deadline, "Redis did not start"
                time.sleep(0.01)
            redis("RPUSH", "list:a", "a", "b", "c")
            redis("RPUSH", "list:b", "d", "e", "f")
            redis("SET", "list:not-a-list", "string")

            assert cli("PING").stdout == b"PONG\n"
            assert b"maxmemory" in cli("CONFIG", "GET", "maxmemory").stdout
            assert cli("SET", "binary", "a b").returncode == 0
            assert cli("--raw", "GET", "binary").stdout == b"a b\n"
            assert cli("--reply-threshold", "1", "LRANGE", "list:a", "0", "-1").returncode == 1
            assert cli("--yes", "--reply-threshold", "1", "LRANGE", "list:a", "0", "-1").returncode == 0
            assert b"PONG" in cli(stdin=b"MULTI\nPING\nEXEC\n").stdout

            bottom = Terminal(socket, start_row=34)
            terminals.append(bottom)
            bottom.send(b"\x1bOP")  # F1 at the bottom of the terminal.
            frame = bottom.expect_frame(
                lambda f: "┌ Help" in f.row(f.y + 1),
                "on-demand editor help below the prompt",
            )
            assert frame.y < 34
            assert any(frame.row(y) == "previous shell output" for y in range(frame.y))
            assert any("Connected" in frame.row(y) for y in range(frame.y + 1, 35))
            bottom.send(b"\x1b")
            bottom.expect_frame(
                lambda f: f.row(f.y) == "rmycli>"
                and all(not f.row(y) for y in range(f.y + 1, 35)),
                "Escape removes help without leaving panel fragments",
            )
            for _ in range(20):
                bottom.send(b"PING\r")
                bottom.expect_frame(
                    lambda f: f.row(f.y) == "rmycli>" and f.row(f.y - 1) == "PONG"
                    and not f.row(f.y + 1),
                    "output scrolls naturally and keeps the prompt after the reply",
                )
            frame = Frame(bottom.screen_data, bottom.height, bottom.start_row)
            assert "previous shell output" in frame.scrolled
            assert "PONG" in frame.scrolled
            bottom.send(b"lrange\x1bOP")
            bottom.expect_frame(lambda f: "┌ Help" in f.row(f.y + 1), "current command help")
            bottom.send(b"\x1b[6~")
            bottom.expect_frame(lambda f: "Connected" in "".join(f.row(y) for y in range(35)),
                                "scrolling command help reaches connection details")
            bottom.send(b"\x1b")
            bottom.expect_frame(lambda f: f.hint_is_inline("lrange", " key start stop"),
                                "help dismissal preserves the draft and inline hint")
            bottom.resize(10, 45)
            bottom.expect_frame(lambda f: f.y < 10 and f.hint_is_inline("lrange", " key start stop"),
                                "resizing preserves input within the new terminal bounds")
            bottom.send(b"\x15")
            bottom.expect_frame(lambda f: f.row(f.y) == "rmycli>", "clear the draft")
            bottom.send(b"\x1bOP")
            bottom.expect_frame(lambda f: "┌ Help" in f.row(f.y + 1),
                                "help before exiting")
            bottom.close()
            terminals.remove(bottom)
            frame = Frame(bottom.screen_data, bottom.height, bottom.start_row)
            assert not any("┌" in frame.row(y) for y in range(10)), "exit left a help panel"
            assert "PONG" in frame.scrolled, "exit lost terminal scrollback"
            assert frame.x == 0 and not frame.row(frame.y), "shell cursor was not restored"

            terminal = Terminal(socket, "--reply-threshold", "1")
            terminals.append(terminal)
            terminal.send(b"PING\r")
            frame = terminal.expect_frame(
                lambda f: f.row(f.y - 1) == "PONG" and f.row(f.y) == "rmycli>",
                "reply followed immediately by the next prompt",
            )
            assert frame.y == 9
            assert frame.row(6) == "previous shell output"
            terminal.send(b"clear\r")
            terminal.expect_frame(
                lambda f: f.row(f.y) == "rmycli>"
                and all(not f.row(y) for y in range(f.y)),
                "clear leaves an empty transcript and prompt",
            )
            terminal.send(b"PING\r")
            terminal.expect("PONG")
            terminal.send(b"lrange")
            frame = terminal.expect_frame(
                lambda f: f.hint_is_inline("lrange", " key start stop"),
                "grey inline argument hint with cursor before hint",
            )
            assert "[0]" not in frame.row(frame.y)
            assert not frame.row(frame.y + 1), "help rendered below input"
            terminal.send(b" list:a ")
            terminal.expect_frame(lambda f: f.hint_is_inline("lrange list:a ", "start stop"), "remaining start and stop")
            terminal.send(b"0 ")
            terminal.expect_frame(lambda f: f.hint_is_inline("lrange list:a 0 ", "stop"), "remaining stop")
            terminal.send(b"-1\r")
            terminal.expect("Continue? [y/N]")
            terminal.send(b"n")
            terminal.expect("Not executed")
            terminal.send(b"\x15LPOP list:\t")
            terminal.expect("┌ 1/2 ")
            assert b"list:not-a-list" not in terminal.data
            frame = terminal.expect_frame(
                lambda f: "┌ 1/2 " in f.row(f.y + 2), "bordered popup below input",
            )
            assert frame.row(frame.y + 2).index("┌") == len("rmycli> LPOP ")
            assert "list:a" in frame.row(frame.y + 3)
            assert "list:b" in frame.row(frame.y + 4)
            terminal.send(b"\t\r")
            terminal.expect("LPOP list:b")
            assert redis("LLEN", "list:b") == 3, "accepting completion executed LPOP"
            terminal.send(b"\r")
            terminal.expect("d")
            deadline = time.monotonic() + 3
            while redis("LLEN", "list:b") == 3:
                assert time.monotonic() < deadline, "second Enter did not execute"
                time.sleep(0.01)
            assert redis("LLEN", "list:b") == 2

            terminal.send(b"SELECT 1\r")
            terminal.expect("rmycli[1]>")
            terminal.send(b"LRANGE list:\t")
            terminal.expect("0 matches")
            terminal.send(b"\x15BLPOP missing 0\r")
            terminal.expect("Executing")
            terminal.send(b"\x03")
            terminal.expect("Connection lost")
            terminal.send(b"\\reconnect\r")
            terminal.expect_frame(lambda f: f.row(f.y) == "rmycli[1]>" and not f.row(f.y + 1), "reconnected prompt")
            terminal.send(b"SUBSCRIBE channel\r")
            terminal.expect("subscribe")
            time.sleep(0.3)  # A quiet stream must not skip the next message.
            redis("PUBLISH", "channel", "after-idle")
            terminal.expect("after-idle")
            terminal.send(b"\x03")
            terminal.expect("Stream ended")
            terminal.send(b"\\reconnect\r")
            terminal.expect_frame(lambda f: f.row(f.y) == "rmycli[1]>" and not f.row(f.y + 1), "reconnected prompt")

            second = Terminal(socket, "--completion-threshold", "1")
            terminals.append(second)
            second.send(b"LRANGE list:\t")
            second.expect("Show candidates? [y/N]")
            second.send(b"y")
            second.expect("┌ 1/2 ")
            second.send(b"\x1b\x15PING\r")
            second.expect("PONG")

            third = Terminal(socket, "--scan-max-batches", "0")
            terminals.append(third)
            third.send(b"LRANGE list:\t")
            third.expect("at least 0 matches (partial scan)")
            third.send(b"\x1b\x15PING\r")
            third.expect("PONG")

            # Enable TCP on this isolated server for a saved instance.
            with sockets.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            redis("CONFIG", "SET", "port", str(port))
            redis("ACL", "SETUSER", "qa", "on", ">qa-test-secret", "~*", "&*", "+@all")
            config = Path(directory) / "rmycli.toml"
            config.write_text(
                '[theme]\ninline-hint = "cyan"\n'
                '[theme.commands]\nlrange = "#12abef"\n'
                'xgroup = "green"\n"XGROUP CREATE" = "red"\n'
                '[instances.qa]\nhost = "127.0.0.1"\n'
                f'port = {port}\nuser = "qa"\n'
                'password-env = "RMYCLI_QA_AUTH"\n',
            )
            auth_env = {"RMYCLI_QA_AUTH": "qa-test-secret"}
            configured = Terminal(None, "--config", str(config), "--instance", "qa",
                                  env=auth_env)
            terminals.append(configured)
            configured.send(b"ACL WHOAMI\r")
            configured.expect_frame(lambda f: any(f.row(y) == "qa" for y in range(f.y)),
                                    "saved instance authenticated as qa")
            configured.send(b"get")
            configured.expect_frame(
                lambda f: f.hint_is_inline("get", " key", ("96", "38;5;14")),
                "configured default hint color",
            )
            configured.send(b"\x15lrange")
            configured.expect_frame(
                lambda f: f.hint_is_inline("lrange", " key start stop", ("38;2;18;171;239",)),
                "configured command hint color",
            )
            configured.send(b"\x15xgroup create")
            configured.expect_frame(
                lambda f: f.row(f.y).startswith("rmycli> xgroup create key group")
                and f.colors.get((f.y, f.x)) in ("91", "38;5;9"),
                "subcommand color overrides parent command",
            )
            configured.send(b"\x15xgroup destroy")
            configured.expect_frame(
                lambda f: f.hint_is_inline("xgroup destroy", " key group", ("92", "38;5;10")),
                "subcommand inherits parent color",
            )
            # Disabling the default account makes missing auxiliary auth fail.
            redis("ACL", "SETUSER", "default", "off")
            configured.send(b"\x15LRANGE list:a\t")
            configured.expect("1 matches")
            configured.send(b"\x15LRANGE list:a 0 -1\r")
            configured.expect_frame(lambda f: any(f.row(y) == "1) a" for y in range(f.y)),
                                    "authenticated execution and auxiliary requests")
            print("Terminal and CLI smoke tests passed")
        finally:
            for terminal in terminals:
                terminal.close()
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
