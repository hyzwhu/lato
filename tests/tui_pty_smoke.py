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
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unicodedata

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/lato").resolve())
requests = []

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        live = "live-progress-demo" in json.dumps(requests[-1])
        if live:
            chunks = [
                b'data: {"choices":[{"delta":{"reasoning_content":"reasoning-start-marker\\n"}}]}\n\n',
                b'data: {"choices":[{"delta":{"reasoning_content":"reasoning-tail-marker"}}]}\n\n',
                b'data: {"choices":[{"delta":{"content":"live-final-answer"}}]}\n\ndata: [DONE]\n\n',
            ]
        else:
            chunks = [b'data: {"choices":[{"delta":{"content":"switched-ok"}}]}\n\ndata: [DONE]\n\n']
        body = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(chunk)
            self.wfile.flush()
            if live:
                time.sleep(0.8)

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix="lato-tui-smoke-") as root:
    home = Path(root) / "home"
    home.mkdir()
    (home / "models.json").write_text(json.dumps({"models": [{
        "provider": "fixture", "id": "fixture-model", "api": "openai-completions",
        "base_url": f"http://127.0.0.1:{server.server_port}/v1", "env": "LATO_FIXTURE_KEY"
    }]}))
    (Path(root) / "context note.txt").write_text("reference-marker-indigo")
    subprocess.run(["git", "init", "-q"], cwd=root, check=True, capture_output=True)
    plugin = Path(root) / "fixture-plugin"
    (plugin / "skills" / "inspect").mkdir(parents=True)
    (plugin / "plugin.json").write_text(json.dumps({"name": "demo", "skills": "skills"}))
    (plugin / "skills" / "inspect" / "SKILL.md").write_text(
        "---\nname: inspect\ndescription: Inspect attached source\nargument-hint: <file>\ndisable-model-invocation: true\n---\nSkill-marker-violet: inspect $1."
    )
    def launch(test_mode, extra_args=()):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(root)
            # Do not inherit credentials into the child smoke process.
            env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR") if key in os.environ}
            env.update(LATO_HOME=str(home), LATO_FIXTURE_KEY="fixture", TERM="xterm-256color")
            if test_mode:
                env["LATO_TUI_TEST"] = "1"
            os.execve(binary, [binary, "--lang", "en", "--plugin-dir", str(plugin), *extra_args], env)
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

    def screen_text():
        # Ratatui sends cell diffs: stripping ANSI loses unchanged characters.
        # Reconstruct cursor-addressed cells for reliable visible-text assertions.
        cells = [[" " for _ in range(120)] for _ in range(30)]
        x = y = 0
        tokens = re.findall(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07)|\x1b.|[^\x1b]", output.decode("utf-8", "replace"), re.S)
        for token in tokens:
            if token.startswith("\x1b["):
                raw, code = token[2:-1], token[-1]
                if raw.startswith("?"):
                    continue
                values = [int(value) if value.isdigit() else 0 for value in raw.split(";")]
                n = values[0] or 1
                if code in "Hf":
                    y = min(29, max(0, n - 1))
                    x = min(119, max(0, (values[1] if len(values) > 1 else 1) - 1))
                elif code == "A": y = max(0, y - n)
                elif code == "B": y = min(29, y + n)
                elif code == "C": x = min(119, x + n)
                elif code == "D": x = max(0, x - n)
                elif code == "G": x = min(119, n - 1)
                elif code == "d": y = min(29, n - 1)
                elif code == "J" and values[0] in (2, 3): cells = [[" "] * 120 for _ in range(30)]
                elif code == "K":
                    a, b = (0, 120) if values[0] == 2 else ((0, x + 1) if values[0] == 1 else (x, 120))
                    cells[y][a:b] = [" "] * (b - a)
                continue
            if token.startswith("\x1b"): continue
            if token == "\r": x = 0
            elif token == "\n": y = min(29, y + 1)
            elif token == "\b": x = max(0, x - 1)
            elif token.isprintable():
                if x >= 120: x, y = 0, min(29, y + 1)
                if unicodedata.combining(token):
                    if x: cells[y][x - 1] += token
                    continue
                cells[y][x] = token
                size = 2 if unicodedata.east_asian_width(token) in ("W", "F") else 1
                if size == 2 and x + 1 < 120: cells[y][x + 1] = ""
                x += size
        return "\n".join("".join(row) for row in cells).encode()

    def expect(text, start=0):
        deadline = time.monotonic() + 8
        while normalized(text.encode()) not in normalized(output[start:]) and normalized(text.encode()) not in normalized(screen_text()) and time.monotonic() < deadline:
            drain(0.1)
        assert normalized(text.encode()) in normalized(output[start:]) or normalized(text.encode()) in normalized(screen_text()), f"missing {text!r}: {bytes(output[-2000:])!r}"
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
        # Completion selects without sending, then actual file contents reach the provider.
        count = len(requests)
        start = send("@context")
        expect("context note.txt", start)
        send("\t")
        assert len(requests) == count, "Tab completion submitted a prompt"
        start = send("explain this\r")
        expect("switched-ok", start)
        assert "reference-marker-indigo" in json.dumps(requests[-1]), "file content absent from request"
        start = send("/skills\r")
        expect("/demo:inspect", start)
        send("demo:inspect\t")
        start = send('@"context note.txt" \r')
        expect("switched-ok", start)
        # The skill request is usually requests[-1] once the response is
        # visible, but response rendering can beat the request bookkeeping;
        # poll briefly instead of asserting a snapshot.
        deadline = time.monotonic() + 5
        while "Skill-marker-violet" not in json.dumps(requests[-1]) and time.monotonic() < deadline:
            drain(0.1)
        assert "Skill-marker-violet" in json.dumps(requests[-1]), "skill body absent from request"
        assert "reference-marker-indigo" in json.dumps(requests[-1]), "skill attachment missing"
        count = len(requests)
        send("first line\x1b\rsecond line")
        assert len(requests) == count, "Alt-Enter submitted a prompt"
        start = send("\r")
        expect("switched-ok", start)
        # Poll briefly: the request may land just after the response renders.
        deadline = time.monotonic() + 5
        while "first line\\nsecond line" not in json.dumps(requests[-1]) and time.monotonic() < deadline:
            drain(0.1)
        assert "first line\\nsecond line" in json.dumps(requests[-1]), "multiline prompt lost newline"
        start = send("\x1b[A")
        expect("second line", start)
        send("\x15")
        start = send("\x0b")
        send("permissions")
        send("\r")
        expect("Sandbox: workspace", start)
        print("PASS: file completion, real attachments, explicit skills, multiline editing, history and searchable palette")
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
        start = send("live-progress-demo\r")
        expect("reasoning-start-marker", start)
        expect("Thinking", start)
        assert b"context:" in normalized(screen_text()), "context disappeared during reasoning"
        expect("reasoning-tail-marker", start)
        expect("live-final-answer", start)
        expect("Completed", start)
        assert b"reasoning-start-marker" not in screen_text(), "completed reasoning did not collapse"
        send("\x1bOQ")  # F2
        expect("reasoning-start-marker")
        assert b"context:" in normalized(screen_text()), "context disappeared after completion"
        print("PASS: incremental reasoning, completion folding, F2 disclosure and persistent context")
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
        os.close(fd)
        output.clear()

        def lato_cmd(*args):
            env = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR") if key in os.environ}
            env.update(LATO_HOME=str(home), LATO_FIXTURE_KEY="fixture")
            result = subprocess.run(
                [binary, *args], cwd=root, env=env, capture_output=True, text=True
            )
            assert result.returncode == 0, result.stderr or result.stdout
            return result

        def session_ids():
            body = json.loads(lato_cmd("sessions", "--json").stdout)
            return [item["sessionId"] for item in body["sessions"]]

        lato_cmd("-p", "unique-resume-title-marker")
        unique_id = session_ids()[0]
        lato_cmd("sessions", "rename", unique_id, "Unique Resume Title")
        pid, fd = launch(False, ["resume", "Unique Resume Title", "--sandbox", "workspace"])
        expect("Trust this folder")
        assert b"Several sessions share" not in normalized(output), "unique title opened a chooser"
        send("Yes\r")
        start = send("what did I ask you to remember?\r")
        expect("switched-ok", start)
        assert "unique-resume-title-marker" in json.dumps(requests[-1]), "unique title resume lost context"
        send("/exit\r")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        os.close(fd)
        output.clear()
        print("PASS: unique exact title resumes without a chooser")

        lato_cmd("-p", "duplicate-older-resume-marker")
        time.sleep(0.02)
        lato_cmd("-p", "duplicate-newer-resume-marker")
        dup_ids = session_ids()[:2]
        for session_id in reversed(dup_ids):
            lato_cmd("sessions", "rename", session_id, "Shared Resume Title")
        pid, fd = launch(False, ["resume", "Shared Resume Title", "--sandbox", "workspace"])
        expect("Several sessions share")
        send("\x1b")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        os.close(fd)
        output.clear()
        print("PASS: duplicate title chooser cancel exits without starting a session")

        pid, fd = launch(False, ["resume", "Shared Resume Title", "--sandbox", "workspace"])
        expect("Several sessions share")
        assert b"Trust this folder" not in normalized(output), "chooser was not shown before trust"
        send("\x1b[B")
        send("\r")
        expect("Trust this folder")
        send("Yes\r")
        start = send("what did I ask you to remember?\r")
        expect("switched-ok", start)
        payload = json.dumps(requests[-1])
        assert "duplicate-older-resume-marker" in payload, "chooser did not resume the older duplicate"
        assert "duplicate-newer-resume-marker" not in payload
        send("/exit\r")
        drain(0.5)
        _, status = os.waitpid(pid, 0)
        pid = None
        assert os.waitstatus_to_exitcode(status) == 0
        print("PASS: duplicate titles choose the selected session after recency ordering")
    finally:
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)
        server.shutdown()
