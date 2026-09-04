#!/usr/bin/env python3
"""Unix PTY regression smoke: python3 tests/tui_pty_smoke.py [path/to/lato].

Uses an isolated home, a local HTTP fixture, and no live provider credentials.
"""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import threading
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/lato").resolve())
requests = []

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        body = b'data: {"choices":[{"delta":{"content":"switched-ok"}}]}\n\ndata: [DONE]\n\n'
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix="lato-tui-smoke-") as root:
    home = Path(root) / "home"
    home.mkdir()
    (home / "models.json").write_text(json.dumps({"models": [{
        "provider": "fixture", "id": "fixture-model", "api": "openai-completions",
        "base_url": f"http://127.0.0.1:{server.server_port}/v1", "env": "LATO_FIXTURE_KEY"
    }]}))
    def launch(test_mode, extra_args=()):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(root)
            # Do not inherit credentials into the child smoke process.
            env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR") if key in os.environ}
            env.update(LATO_HOME=str(home), LATO_FIXTURE_KEY="fixture", TERM="xterm-256color")
            if test_mode:
                env["LATO_TUI_TEST"] = "1"
            os.execve(binary, [binary, "--lang", "en", *extra_args], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        return pid, fd

    pid, fd = launch(True)
    output = bytearray()

    def drain(duration=0.3):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([fd], [], [], min(0.05, max(0, deadline - time.monotonic())))[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                output.extend(data)

    def normalized(data):
        return re.sub(rb"\s+", b"", re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data))

    def expect(text, start=0):
        deadline = time.monotonic() + 8
        while normalized(text.encode()) not in normalized(output[start:]) and time.monotonic() < deadline:
            drain(0.1)
        assert normalized(text.encode()) in normalized(output[start:]), f"missing {text!r}: {bytes(output[-2000:])!r}"
        assert b"\x1b[?1049l" not in output, "TUI left alternate screen during operation"

    def send(text):
        checkpoint = len(output)
        os.write(fd, text.encode())
        drain()
        return checkpoint

    try:
        expect("Enter a prompt")
        start = send("/")
        expect("/help", start)
        start = send("mo")
        expect("/model", start)
        send("\r")
        start = send("\r")
        expect("Provider", start)
        send("\x1b")
        start = send("/permissions\r")
        expect("Sandbox: workspace", start)
        expect("Automatic approval", start)
        send("remember marker-saffron\r")
        expect("hi")
        start = send("/model\r")
        expect("Provider", start)
        send("\x1b")
        start = send("/model\r")
        expect("Provider", start)
        send("fixture\r")
        expect("fixture-model", start)
        send("\r")
        deadline = time.monotonic() + 5
        while not (home / "config.json").exists() and time.monotonic() < deadline:
            drain()
        assert json.loads((home / "config.json").read_text())["default_model"] == "fixture/fixture-model"
        start = send("what did I say?\r")
        expect("switched-ok", start)
        assert "marker-saffron" in json.dumps(requests[-1]), "model switch lost context"
        session_count = len(list((home / "sessions").glob("*")))
        start = send("/model\r")
        expect("Provider", start)
        send("openai\r")
        expect("Authentication", start)
        send("API key\r")
        expect("API key for", start)
        send("secret-should-never-echo")
        assert b"secret-should-never-echo" not in normalized(output)
        send("\x1b")
        assert len(list((home / "sessions").glob("*"))) == session_count
        start = send("/sessions\r")
        expect("type to filter", start)
        send("\x1b")
        start = send("/doctor\r")
        expect("Lato doctor", start)
        start = send("/login\r")
        expect("no interactive authentication method", start)
        assert b"\x1b[?1049l" not in output
        send("/exit\r")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        assert output.count(b"\x1b[?1049h") == 1
        assert output.count(b"\x1b[?1049l") == 1
        print("PASS: one TUI lifetime; model switch preserves context; cancellation, masked credentials, sessions and doctor stay inside TUI")
        os.close(fd)
        (home / "config.json").unlink()
        output.clear()
        previous_sessions = set((home / "sessions").iterdir())
        pid, fd = launch(False)
        expect("Provider")
        send("fixture\r")
        expect("fixture-model")
        send("\r")
        expect("Trust this folder")
        send("\r")
        expect("Choose sandbox scope")
        send("off\r")
        expect("Enter a prompt")
        start = send("/permissions\r")
        expect("Sandbox: off", start)
        expect("Ask before mutations", start)
        start = send("remember permission-resume-marker\r")
        expect("switched-ok", start)
        new_sessions = set((home / "sessions").iterdir()) - previous_sessions
        assert len(new_sessions) == 1, new_sessions
        resume_id = new_sessions.pop().name
        send("/exit\r")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        assert output.count(b"\x1b[?1049h") == 1
        assert output.count(b"\x1b[?1049l") == 1
        print("PASS: first-run model configuration and folder trust stay inside TUI")
        os.close(fd)
        output.clear()
        pid, fd = launch(False, ["resume", resume_id, "--sandbox", "read-only"])
        expect("Trust this folder")
        send("Yes\r")
        start = send("/permissions\r")
        expect("Sandbox: read-only", start)
        expect("Automatic approval", start)
        assert b"Choosesandboxscope" not in normalized(output), "explicit sandbox should skip picker"
        start = send("what did I ask you to remember?\r")
        expect("switched-ok", start)
        assert "permission-resume-marker" in json.dumps(requests[-1]), "resume lost context"
        send("/exit\r")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        print("PASS: resume uses explicit read-only instead of previous off, independently of trust, and preserves context")
    finally:
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)
        server.shutdown()
