#!/usr/bin/env python3
"""Exercise the real terminal and binary against an isolated Redis server.

Run after cargo build. Requires Python 3, redis-server, redis-cli, and a Unix PTY.
No third-party Python dependencies or existing Redis instance are needed.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/rmycli"


class Frame:
    """Read the cursor moves, clears, and colors emitted by our renderer."""
    def __init__(self, data):
        end = data.rfind(b"\x1b[?25h")
        start = data.rfind(b"\x1b[2J", 0, end)
        self.cells = {}
        self.colors = {}
        self.x = self.y = 0
        color = ""
        if start < 0:
            return
        text = data[start:end].decode("utf-8", errors="replace")
        for part in re.split(r"(\x1b\[[0-?]*[ -/]*[@-~])", text):
            if part.startswith("\x1b["):
                if part.endswith("H"):
                    row, column = (int(n) for n in part[2:-1].split(";"))
                    self.y, self.x = row - 1, column - 1
                elif part.endswith("m"):
                    color = part[2:-1]
                elif part == "\x1b[2J":
                    self.cells.clear()
                    self.colors.clear()
            else:
                for char in part:
                    self.cells[self.y, self.x] = char
                    self.colors[self.y, self.x] = color
                    self.x += 1

    def row(self, row):
        width = max((x + 1 for y, x in self.cells if y == row), default=0)
        return "".join(self.cells.get((row, x), " ") for x in range(width)).rstrip()

    def hint_is_inline(self, typed, hint):
        prompt = "rmycli> "
        return (self.row(self.y) == prompt + typed + hint
                and self.x == len(prompt + typed)
                and (not hint or self.colors.get((self.y, self.x)) in ("90", "38;5;8")))


class Terminal:
    def __init__(self, socket, *options):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))
        self.process = subprocess.Popen(
            [str(BINARY), "-s", socket, *options], stdin=slave, stdout=slave,
            stderr=slave, env={**{k: v for k, v in os.environ.items() if k != "NO_COLOR"},
                 "TERM": "xterm-256color"},
        )
        os.close(slave)
        self.data = b""
        self.expect("Connected")

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
                self.data += os.read(self.master, 65536)
        return self.data

    def expect_frame(self, predicate, description, timeout=5):
        deadline = time.monotonic() + timeout
        while True:
            frame = Frame(self.data)
            if predicate(frame):
                return frame
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"Missing {description}: {self.data[-6000:]!r}"
            ready, _, _ = select.select([self.master], [], [], remaining)
            if ready:
                self.data += os.read(self.master, 65536)

    def close(self):
        if self.process.poll() is None:
            os.write(self.master, b"\x04")
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        os.close(self.master)
        assert self.process.returncode == 0


def main():
    with tempfile.TemporaryDirectory(prefix="rmycli-terminal-") as directory:
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
            return subprocess.run([str(BINARY), "-s", socket, *args],
                                  input=stdin, capture_output=True, timeout=5)

        try:
            deadline = time.monotonic() + 5
            while not Path(socket).exists():
                assert time.monotonic() < deadline, "Redis did not start"
                time.sleep(0.01)
            redis("RPUSH", "list:a", "a", "b", "c")
            redis("RPUSH", "list:b", "d", "e", "f")
            redis("SET", "list:not-a-list", "string")

            assert cli("PING").stdout == b"PONG\n"
            assert cli("SET", "binary", "a b").returncode == 0
            assert cli("--raw", "GET", "binary").stdout == b"a b\n"
            assert cli("--reply-threshold", "1", "LRANGE", "list:a", "0", "-1").returncode == 1
            assert cli("--yes", "--reply-threshold", "1", "LRANGE", "list:a", "0", "-1").returncode == 0
            assert b"PONG" in cli(stdin=b"MULTI\nPING\nEXEC\n").stdout

            terminal = Terminal(socket, "--reply-threshold", "1")
            terminals.append(terminal)
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
            terminal.expect("Tab/Shift-Tab cycle")
            assert b"list:not-a-list" not in terminal.data
            frame = terminal.expect_frame(
                lambda f: "┌ 1/2 " in f.row(f.y - 4), "bordered popup above input",
            )
            assert frame.row(frame.y - 4).index("┌") == len("rmycli> LPOP ")
            assert "list:a" in frame.row(frame.y - 3)
            assert "list:b" in frame.row(frame.y - 2)
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
            terminal.expect("Connected")
            terminal.send(b"SUBSCRIBE channel\r")
            terminal.expect("subscribe")
            time.sleep(0.3)  # A quiet stream must not skip the next message.
            redis("PUBLISH", "channel", "after-idle")
            terminal.expect("after-idle")
            terminal.send(b"\x03")
            terminal.expect("Stream ended")
            terminal.send(b"\\reconnect\r")
            terminal.expect("Connected")

            second = Terminal(socket, "--completion-threshold", "1")
            terminals.append(second)
            second.send(b"LRANGE list:\t")
            second.expect("Show candidates? [y/N]")
            second.send(b"y")
            second.expect("Tab/Shift-Tab cycle")
            second.send(b"\x1b\x15PING\r")
            second.expect("PONG")

            third = Terminal(socket, "--scan-max-batches", "0")
            terminals.append(third)
            third.send(b"LRANGE list:\t")
            third.expect("at least 0 matches (partial scan)")
            third.send(b"\x1b\x15PING\r")
            third.expect("PONG")
            print("Terminal and CLI smoke tests passed")
        finally:
            for terminal in terminals:
                terminal.close()
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
