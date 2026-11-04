#!/usr/bin/env python3
"""Exercise the real terminal and binary against an isolated Redis server.

Run after cargo build, or set RMYCLI_TEST_BINARY to test a distribution binary.
Requires Python 3, redis-server, redis-cli, and a Unix PTY.
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
import shlex
import shutil
import socket as sockets
import struct
import subprocess
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get(
    "RMYCLI_TEST_BINARY", ROOT / "target/debug/rmycli",
)).resolve()


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
        prompt = self.row(self.y).split(">", 1)[0] + "> "
        return (self.row(self.y) == prompt + typed + hint
                and self.x == len(prompt + typed)
                and (not hint or self.colors.get((self.y, self.x)) in colors))


class Terminal:
    def __init__(self, socket, *options, env=None, start_row=7, startup_notice=None, endpoint=None, user="default"):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))
        connection = ["-s", socket] if socket else []
        config = [] if "--config" in options else ["--no-config"]
        self.process = subprocess.Popen(
            [str(BINARY), *connection, *config, *options], stdin=slave, stdout=slave,
            stderr=slave, env={**{k: v for k, v in os.environ.items()
                                 if k not in ("NO_COLOR", "REDIS_URL", "REDISCLI_AUTH")},
                 "TERM": "xterm-256color",
                 **({"XDG_CONFIG_HOME": str(Path(socket).parent)} if socket else {}),
                 **(env or {})},
        )
        os.close(slave)
        self.data = b""
        self.screen_data = b""
        self.height = 35
        self.start_row = start_row
        started = time.monotonic()
        prompt = f"{user}@{endpoint or socket}>"
        self.expect(prompt)
        if b"Loading server command metadata" in self.screen_data:
            assert time.monotonic() - started >= 0.2, "startup status flashed"
        self.expect_frame(lambda f: f.row(f.y) == prompt
                          and not f.row(f.y + 1), "quiet connected prompt")
        assert b"\x1b[?1049h" not in self.screen_data
        assert b"Connected" not in self.screen_data
        assert b"Tab: complete" not in self.screen_data
        frame = Frame(self.screen_data, self.height, self.start_row)
        if startup_notice:
            self.expect(startup_notice)
            assert b"COMMAND unavailable" not in self.screen_data
        else:
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


def password_prompt(socket, cancel=False):
    """Verify --askpass hides input and restores terminal echo on both paths."""
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    env = {k: v for k, v in os.environ.items() if k != "REDIS_URL"}
    env["REDISCLI_AUTH"] = "wrong-environment-password"
    process = subprocess.Popen(
        [str(BINARY), "--no-config", "-s", socket, "--askpass", "--user",
         "ask-test", "-a", "wrong-flag-password", "PING"],
        stdin=slave, stdout=slave, stderr=slave, env=env,
    )
    data = b""
    try:
        deadline = time.monotonic() + 5
        while b"Password:" not in data:
            assert time.monotonic() < deadline, "missing password prompt"
            if select.select([master], [], [], 0.1)[0]:
                data += os.read(master, 65536)
        while termios.tcgetattr(slave)[3] & termios.ECHO:
            assert time.monotonic() < deadline, "password echo was not disabled"
            time.sleep(0.01)
        os.write(master, b"\x03" if cancel else b"hidden-test-password\r")
        process.wait(timeout=5)
        while select.select([master], [], [], 0)[0]:
            data += os.read(master, 65536)
        assert b"hidden-test-password" not in data, data
        assert b"wrong-flag-password" not in data, data
        assert bool(termios.tcgetattr(slave)[3] & termios.ECHO) == bool(original[3] & termios.ECHO)
        assert process.returncode == (1 if cancel else 0), data
        assert (b"cancelled" if cancel else b"PONG") in data, data
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(slave)
        os.close(master)


def tmux_history(socket, directory, redis):
    """Check ordinary Ctrl-R with a private tmux server and default key handling."""
    if not shutil.which("tmux"):
        print("Skipping tmux history check: tmux is not installed")
        return
    tmux = ["tmux", "-S", str(Path(directory) / "tmux.sock"), "-f", "/dev/null"]
    env = {k: v for k, v in os.environ.items()
           if k not in ("REDIS_URL", "REDISCLI_AUTH", "TMUX")}
    env["XDG_CONFIG_HOME"] = str(Path(directory) / "tmux-config")

    def run(*args):
        return subprocess.check_output([*tmux, *args], env=env, text=True)

    def expect(*texts):
        deadline = time.monotonic() + 5
        while True:
            pane = run("capture-pane", "-p", "-t", "history:0.0")
            if all(text in pane for text in texts):
                return
            assert time.monotonic() < deadline, f"Missing {texts!r} in tmux: {pane!r}"
            time.sleep(0.02)

    def keys(*names):
        run("send-keys", "-t", "history:0.0", *names)

    def text(value):
        run("send-keys", "-t", "history:0.0", "-l", value)

    try:
        run("new-session", "-d", "-s", "history", "-x", "120", "-y", "35",
            shlex.join([str(BINARY), "--no-config", "-s", socket]))
        expect(f"default@{socket}>")
        for suffix in ("user", "upper"):
            text(f"INCR tmux-history:{suffix}")
            keys("Enter")
            expect("(integer) 1")
        keys("C-r")
        text("husr")
        expect("history> husr", "1 matches", "INCR tmux-history:user")
        keys("Home")
        text("'")
        expect("history> 'husr", "0 matches")
        keys("Home", "Delete")
        expect("history> husr", "1 matches")
        keys("Enter")
        expect(f"default@{socket}> INCR tmux-history:user")
        assert redis("GET", "tmux-history:user") == "1", "recall executed in tmux"
        keys("C-u")
        text("GET draft")
        keys("Left", "Left", "Left", "C-r", "C-u")
        text("missing")
        expect("history> missing", "0 matches")
        keys("C-c")
        text("!")
        expect(f"default@{socket}> GET dr!aft")
    finally:
        subprocess.run([*tmux, "kill-server"], env=env, capture_output=True, check=False)


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

            redis("ACL", "SETUSER", "ask-test", "on", ">hidden-test-password", "~*", "+@all")
            password_prompt(socket)
            password_prompt(socket, cancel=True)

            tmux_history(socket, directory, redis)

            history = Terminal(socket)
            terminals.append(history)
            for suffix in ("user", "upper"):
                history.send(f"INCR history:{suffix}\r".encode())
                history.expect("(integer) 1")
            history.send(b"\x12husr")  # Legacy Ctrl-R byte, no extended keyboard protocol.
            history.expect_frame(lambda f: f.row(f.y) == "history> husr"
                                 and "1 matches" in f.row(f.y + 1), "fuzzy history")
            history.send(b"\x01'")
            history.expect("0 matches")
            history.send(b"\x7f")
            history.expect("1 matches")
            history.send(b"\r")
            history.expect_frame(lambda f: f.row(f.y).startswith(f"default@{socket}> INCR history:user")
                                 and not f.row(f.y + 1), "accepted history command")
            assert redis("GET", "history:user") == "1", "history acceptance executed"
            history.send(b"\r")
            history.expect("(integer) 2")
            history.send(b"GET draft\x1b[D\x1b[D\x1b[D\x12\x15missing")
            history.expect("0 matches")
            history.send(b"\x03!")
            history.expect_frame(lambda f: f.row(f.y).startswith(f"default@{socket}> GET dr!aft"),
                                 "history cancellation restored draft cursor")
            history.close()
            terminals.remove(history)

            restored_history = Terminal(socket)
            terminals.append(restored_history)
            restored_history.send(b"\x1b[A")
            restored_history.expect_frame(
                lambda f: f.row(f.y).startswith(f"default@{socket}> INCR history:user"),
                "Up recalls a command from the previous session",
            )
            assert redis("GET", "history:user") == "2", "restoring history executed"
            restored_history.send(b"\x15\x12husr")
            restored_history.expect("1 matches")
            restored_history.send(b"\r")
            restored_history.expect_frame(
                lambda f: f.row(f.y).startswith(f"default@{socket}> INCR history:user"),
                "Ctrl-R recalls persistent history",
            )
            restored_history.close()
            terminals.remove(restored_history)
            other_socket = str(Path(directory) / "other.sock")
            Path(other_socket).symlink_to(socket)
            other_history = Terminal(other_socket)
            terminals.append(other_history)
            other_history.send(b"\x12husr")
            other_history.expect("0 matches")
            other_history.send(b"\x03")
            other_history.close()
            terminals.remove(other_history)

            flat_config = Path(directory) / "flat-history.toml"
            flat_config.write_text("history-per-server = false\n")
            flat_history = Terminal(other_socket, "--config", str(flat_config))
            terminals.append(flat_history)
            flat_history.send(b"\x1b[A")
            flat_history.expect_frame(
                lambda f: f.row(f.y).startswith(f"default@{other_socket}> INCR history:user"),
                "flat history recalls another endpoint's command",
            )
            flat_history.send(b"\x15\x12husr")
            flat_history.expect("1 matches")
            flat_history.send(b"\x03")
            flat_history.close()
            terminals.remove(flat_history)
            assert redis("GET", "history:user") == "2", "flat history executed"

            tracking = Terminal(socket, "-3")
            terminals.append(tracking)
            for command, reply in [(b"SET tracked original\r", "OK"),
                                   (b"CLIENT TRACKING ON\r", "OK"),
                                   (b"GET tracked\r", "original")]:
                tracking.send(command)
                tracking.expect(reply)
            tracking.send(b"GET draft\x1b[D")
            tracking.expect_frame(
                lambda f: f.row(f.y) == f"default@{socket}> GET draft"
                and f.x == len(f"default@{socket}> GET draf"), "draft before invalidation",
            )
            tracking.data = b""
            redis("SET", "tracked", "changed")
            frame = tracking.expect_frame(
                lambda f: any(f.row(y) == "-> invalidate: 'tracked'" for y in range(f.y))
                and f.row(f.y) == f"default@{socket}> GET draft"
                and f.x == len(f"default@{socket}> GET draf"), "idle push preserves input",
            )
            row = next(y for y in range(frame.y) if frame.row(y) == "-> invalidate: 'tracked'")
            assert frame.colors[row, 0] == "1", "push was not bold"
            tracking.send(b"\x05\x15PING\r")
            tracking.expect("PONG")

            bottom = Terminal(socket, "-3", start_row=34)
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
                lambda f: f.row(f.y) == f"default@{socket}>"
                and all(not f.row(y) for y in range(f.y + 1, 35)),
                "Escape removes help without leaving panel fragments",
            )
            for _ in range(20):
                bottom.send(b"PING\r")
                bottom.expect_frame(
                    lambda f: f.row(f.y) == f"default@{socket}>" and f.row(f.y - 1) == "PONG"
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
            bottom.resize(10, 90)
            bottom.expect_frame(lambda f: f.y < 10 and f.hint_is_inline("lrange", " key start stop"),
                                "resizing preserves input within the new terminal bounds")
            bottom.send(b"\x15")
            bottom.expect_frame(lambda f: f.row(f.y) == f"default@{socket}>", "clear the draft")
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
                lambda f: f.row(f.y - 1) == "PONG" and f.row(f.y) == f"default@{socket}>",
                "reply followed immediately by the next prompt",
            )
            assert frame.y == 9
            assert frame.row(6) == "previous shell output"
            terminal.send(b"clear\r")
            terminal.expect_frame(
                lambda f: f.row(f.y) == f"default@{socket}>"
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
            assert frame.row(frame.y + 2).index("┌") == len(f"default@{socket}> LPOP ")
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
            terminal.expect(f"default@{socket}[1]>")
            terminal.send(b"LRANGE list:\t")
            terminal.expect("0 matches")
            terminal.send(b"\x15BLPOP missing 0\r")
            terminal.expect("Executing")
            terminal.send(b"\x03")
            terminal.expect("Connection lost")
            terminal.send(b"\\reconnect\r")
            terminal.expect_frame(lambda f: f.row(f.y) == f"default@{socket}[1]>" and not f.row(f.y + 1), "reconnected prompt")
            terminal.send(b"SUBSCRIBE channel\r")
            terminal.expect("subscribe")
            time.sleep(0.3)  # A quiet stream must not skip the next message.
            redis("PUBLISH", "channel", "after-idle")
            terminal.expect("after-idle")
            terminal.send(b"\x03")
            terminal.expect("Stream ended")
            terminal.send(b"\\reconnect\r")
            terminal.expect_frame(lambda f: f.row(f.y) == f"default@{socket}[1]>" and not f.row(f.y + 1), "reconnected prompt")

            second = Terminal(socket, "--completion-threshold", "1")
            terminals.append(second)
            second.send(b"LRANGE list:\t")
            second.expect("Show candidates? [y/N]")
            second.send(b"y")
            second.expect("┌ 1/2 ")
            second.send(b"\x1b\x15PING\r")
            second.expect("PONG")

            third = Terminal(socket, "-3", "--scan-max-batches", "0")
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
            auth_env = {"RMYCLI_QA_AUTH": "qa-test-secret", "XDG_CONFIG_HOME": directory}
            configured = Terminal(None, "--config", str(config), "--instance", "qa",
                                  env=auth_env, endpoint=f"127.0.0.1:{port}", user="qa")
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
                lambda f: f.row(f.y).startswith(f"qa@127.0.0.1:{port}> xgroup create key group")
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
            auth_settings = Path(directory) / "auth-settings.toml"
            auth_settings.write_text('save-auth = "local"\n')
            saved_auth = Path(directory) / "rmycli-auth.toml"
            unauthenticated = Terminal(
                socket, "--config", str(auth_settings),
                env={"XDG_CONFIG_HOME": directory},
                startup_notice="Authentication required",
            )
            terminals.append(unauthenticated)
            unauthenticated.send(b"AUTH qa wrong\r")
            unauthenticated.expect("invalid username-password")
            assert not saved_auth.exists()
            unauthenticated.send(b"AUTH qa qa-test-secret\r")
            unauthenticated.expect_frame(
                lambda f: f.row(f.y - 1) == "OK" and f.row(f.y) == f"qa@{socket}>",
                "successful authentication returns to the prompt",
            )
            assert saved_auth.exists()
            # The worker refreshes metadata after AUTH and before the next command.
            unauthenticated.send(b"PING\r")
            unauthenticated.expect_frame(
                lambda f: f.row(f.y - 1) == "PONG",
                "authenticated command after metadata refresh",
            )
            unauthenticated.send(b"LRANGE list:a\t")
            unauthenticated.expect("1 matches")
            restored = Terminal(socket, "--config", str(auth_settings),
                                env={"XDG_CONFIG_HOME": directory}, user="qa")
            terminals.append(restored)
            restored.send(b"ACL WHOAMI\r")
            restored.expect_frame(lambda f: f.row(f.y - 1) == "qa", "saved auth reused")
            print("Terminal and CLI smoke tests passed")
        finally:
            for terminal in terminals:
                terminal.close()
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
