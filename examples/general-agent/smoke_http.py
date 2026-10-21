#!/usr/bin/env python3
"""Run the real HTTP/MCP chain with a local scripted provider and synthetic data."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import os
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="lato")
    parser.add_argument("--workflow", action="store_true", help="Exercise two delegated agents and background workflow results")
    args = parser.parse_args()
    binary = shutil.which(args.binary)
    if not binary:
        parser.error("lato executable not found")
    token = "fixture-service-token-for-local-smoke-only"
    requests = []
    errors = []
    with tempfile.TemporaryDirectory(prefix="lato-http-business-smoke-") as temporary:
        root = Path(temporary)
        plugin = root / "business-plugin"
        shutil.copytree(Path(__file__).parent / "business-plugin", plugin,
                        ignore=shutil.ignore_patterns("outputs", "__pycache__"))
        home = root / "home"
        home.mkdir()
        (root / "agent.json").write_text(json.dumps({
            "mode": "general",
            "instructions": "Use only the business-demo customer lookup and report modules. All data is synthetic.",
            "tools": ["builtin:search_tool", "builtin:use_tool", "builtin:skill"],
        }), encoding="utf-8")

        if args.workflow:
            (home / "workflows").mkdir()
            shutil.copyfile(Path(__file__).parent / "customer-report.rhai", home / "workflows/customer-report.rhai")
            profile = json.loads((root / "agent.json").read_text())
            profile["tools"].append("builtin:workflow")
            (root / "agent.json").write_text(json.dumps(profile))

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    assert self.path == "/v1/chat/completions"
                    assert self.headers.get("Authorization") == "Bearer fixture-model-key"
                    length = int(self.headers["Content-Length"])
                    assert length <= 1024 * 1024
                    request = json.loads(self.rfile.read(length))
                    requests.append(request)
                    step = len(requests)
                    workflow_arguments = None
                    if args.workflow:
                        messages = request["messages"]
                        delegated = "Delegated task context:" in messages[0]["content"]
                        results = [m for m in messages if m["role"] == "tool"]
                        if delegated:
                            assert "Use only the business-demo" in messages[0]["content"]
                            catalog = json.dumps(request.get("tools", []))
                            assert '"name": "workflow"' not in catalog
                            report_phase = "REPORT_PHASE" in messages[0]["content"]
                            if report_phase:
                                assert "42000" in json.dumps(messages)
                            tool = None if results else ("business-demo__create_customer_report" if report_phase else "business-demo__lookup_customer")
                            arguments = {"customerId": "acme"} if report_phase else {"customer": "Acme"}
                            final_text = ("Report artifact: " + str((plugin / "outputs/acme-report.md").resolve())) if report_phase else "Verified customerId: acme, revenue: 42000 CNY, orders: 12. Synthetic data."
                            if results:
                                assert "42000" in json.dumps(messages)
                        elif not results:
                            tool = "workflow"
                            workflow_arguments = {"action": "list"}
                        elif len(results) == 1:
                            entry = json.loads(results[0]["content"])["workflows"][0]
                            tool = "workflow"
                            workflow_arguments = {"action": "start", "name": entry["id"], "revision": entry["revision"], "agentBudget": 2, "args": {"customer": "Acme"}}
                        else:
                            tool = None
                            final_text = "Business workflow started; inspect workflow-runs for the result."
                    elif step == 1:
                        assert "general-purpose agent" in request["messages"][0]["content"]
                        tool = "business-demo__lookup_customer"
                        arguments = {"customer": "Acme"}
                    elif step == 2:
                        assert "42000" in json.dumps(request["messages"])
                        tool = "business-demo__create_customer_report"
                        arguments = {"customerId": "acme"}
                    elif step == 3:
                        report = plugin / "outputs/acme-report.md"
                        assert report.is_file(), "report tool did not create its artifact"
                        assert "42000 CNY" in report.read_text(encoding="utf-8")
                        tool = None
                    else:
                        raise AssertionError("unexpected extra provider call")
                    if tool:
                        delta = {"tool_calls": [{"index": 0, "id": f"business-{step}", "type": "function",
                                                   "function": {"name": "workflow" if workflow_arguments else "use_tool", "arguments": json.dumps(workflow_arguments if workflow_arguments else {"tool": tool, "arguments": arguments})}}]}
                        frames = [{"choices": [{"delta": delta}]},
                                  {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}]
                    else:
                        frames = [{"choices": [{"delta": {"content": final_text if args.workflow else "Acme synthetic report created: 42000 CNY; 12 orders."}}]}]
                    body = "".join("data: " + json.dumps(frame) + "\n\n" for frame in frames) + "data: [DONE]\n\n"
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(body.encode())))
                    self.end_headers()
                    self.wfile.write(body.encode())
                except Exception as error:
                    errors.append(repr(error))
                    self.send_error(500, "fixture verification failed")

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
        provider_thread.start()
        (home / "models.json").write_text(json.dumps({"models": [{
            "provider": "http-smoke", "id": "business", "api": "openai-completions",
            "base_url": f"http://127.0.0.1:{provider.server_port}/v1", "env": "LATO_SMOKE_MODEL_KEY",
        }]}), encoding="utf-8")
        env = dict(os.environ, LATO_HOME=str(home), LATO_SERVICE_TOKEN=token,
                   LATO_SMOKE_MODEL_KEY="fixture-model-key",
                   NO_PROXY="127.0.0.1,localhost,::1", no_proxy="127.0.0.1,localhost,::1")
        child = subprocess.Popen([binary, "serve", "--model", "http-smoke/business",
                                  "--listen", "127.0.0.1:0", "--agent-config", str(root / "agent.json"),
                                  "--plugin-dir", str(plugin), "--trust-workspace"],
                                 cwd=root, env=env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        lines = queue.Queue()

        def read_stderr():
            for line in child.stderr:
                lines.put(line.rstrip())
            lines.put(None)

        threading.Thread(target=read_stderr, daemon=True).start()
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            deadline = time.monotonic() + 15
            while True:
                line = lines.get(timeout=max(0.1, deadline - time.monotonic()))
                if line is None:
                    raise RuntimeError("service stopped before announcing its address")
                if line.startswith("Lato service listening on "):
                    base = line.removeprefix("Lato service listening on ").split(";")[0]
                    break

            def api(method, path, body=None):
                data = None if body is None else json.dumps(body).encode()
                request = urllib.request.Request(base + path, data=data, method=method,
                                                headers={"Authorization": "Bearer " + token,
                                                         "Content-Type": "application/json"})
                with opener.open(request, timeout=10) as response:
                    return json.load(response)

            session = api("POST", "/v1/sessions", {})["sessionId"]
            operation = api("POST", f"/v1/sessions/{session}/operations",
                            {"text": "查询 Acme 客户并使用业务模块生成报告，说明是演示数据。"})["operationId"]
            deadline = time.monotonic() + 45
            approvals = 0
            while time.monotonic() < deadline:
                status = api("GET", f"/v1/operations/{operation}")
                runs = api("GET", f"/v1/sessions/{session}/workflow-runs")["runs"] if args.workflow else []
                if status["status"] in ("cancelled", "failed"):
                    break
                if status["status"] == "completed" and (not args.workflow or (runs and runs[0]["status"] != "active")):
                    break
                for approval in api("GET", f"/v1/sessions/{session}/approvals")["approvals"]:
                    call = approval.get("arguments", {})
                    assert (approval["tool"] == "builtin:workflow" and call.get("action") in ("list", "start")) or call.get("tool") in ("business-demo__lookup_customer", "business-demo__create_customer_report"), call
                    assert not approval.get("argumentsTruncated")
                    api("POST", f"/v1/sessions/{session}/approvals/{approval['approvalId']}", {"allow": True})
                    approvals += 1
                time.sleep(0.05)
            else:
                raise RuntimeError("business operation timed out")
            if status["status"] != "completed":
                raise AssertionError({"operation": status, "fixtureErrors": errors,
                                      "lastMessages": requests[-1].get("messages", [])[-2:] if requests else []})
            assert len(requests) == (7 if args.workflow else 3), len(requests)
            if args.workflow:
                assert runs[0]["status"] == "complete", runs
                assert runs[0]["agentsUsed"] == 2, runs
                assert runs[0]["result"]["customer"] == "Acme", runs
                assert "acme-report.md" in runs[0]["result"]["report"], runs
            assert not errors, errors
            assert approvals >= 1, "write did not request approval"
            assert "42000 CNY" in (plugin / "outputs/acme-report.md").read_text(encoding="utf-8")
            api("POST", f"/v1/sessions/{session}/close", {})
            print(json.dumps({"status": "passed", "providerCalls": len(requests),
                              "businessModules": ["lookup_customer", "create_customer_report"],
                              "approvals": approvals, "artifactVerified": True, "workflow": args.workflow,
                              "model": "scripted local HTTP fixture", "data": "synthetic"}))
        finally:
            child.terminate()
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            provider.shutdown()
            provider.server_close()


if __name__ == "__main__":
    main()
