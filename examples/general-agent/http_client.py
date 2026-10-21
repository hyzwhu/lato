#!/usr/bin/env python3
"""Minimal application client. POST submissions are deliberately never retried."""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("prompt")
    parser.add_argument("--url", default="http://127.0.0.1:8765")
    parser.add_argument("--allow-writes", action="store_true")
    args = parser.parse_args()
    token = os.environ.get("LATO_SERVICE_TOKEN")
    if not token:
        parser.error("set LATO_SERVICE_TOKEN")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect)

    def request(method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        message = urllib.request.Request(
            args.url.rstrip("/") + path,
            data=data,
            method=method,
            headers={
                "Authorization": "Bearer " + token,
                "Content-Type": "application/json",
            },
        )
        try:
            with opener.open(message, timeout=15) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            try:
                details = json.load(error)
            except (ValueError, OSError):
                details = {"error": {"message": "HTTP " + str(error.code)}}
            raise RuntimeError(details.get("error", {}).get("message", "request failed")) from None

    session = request("POST", "/v1/sessions", {})["sessionId"]
    operation = request("POST", f"/v1/sessions/{session}/operations", {"text": args.prompt})[
        "operationId"
    ]
    print(f"Session: {session}; operation: {operation}", file=sys.stderr)
    printed = False
    try:
        while True:
            status = request("GET", f"/v1/operations/{operation}")
            if status["status"] in ("completed", "cancelled", "failed"):
                if status["status"] == "completed":
                    if not printed:
                        print(status["result"]["text"])
                        if status["result"]["truncated"]:
                            print("Result exceeds the HTTP limit; consult the session journal.", file=sys.stderr)
                        printed = True
                    runs = request("GET", f"/v1/sessions/{session}/workflow-runs")["runs"]
                    if not any(run["status"] == "active" for run in runs):
                        for run in runs:
                            print(json.dumps(run, ensure_ascii=False))
                        paused = any(run["status"] not in ("complete", "failed", "cancelled", "interrupted") for run in runs)
                        if paused:
                            print(f"Workflow needs attention. Session {session} remains open; resume or stop it through workflow-runs routes.", file=sys.stderr)
                            return 2
                        request("POST", f"/v1/sessions/{session}/close", {})
                        return int(any(run["status"] != "complete" for run in runs))
                else:
                    print(json.dumps(status, ensure_ascii=False), file=sys.stderr)
                    request("POST", f"/v1/sessions/{session}/close", {})
                    return 1
            pending = request("GET", f"/v1/sessions/{session}/approvals")["approvals"]
            for approval in pending:
                allow = args.allow_writes
                if not allow and sys.stdin.isatty():
                    print(json.dumps(approval.get("arguments"), ensure_ascii=False))
                    if approval.get("argumentsTruncated"):
                        print("Arguments exceed the approval display limit; review the application/journal before allowing.")
                    answer = input(approval["summary"] + " — allow this call? [y/N] ")
                    allow = answer.strip().lower() in ("y", "yes")
                request(
                    "POST",
                    f"/v1/sessions/{session}/approvals/{approval['approvalId']}",
                    {"allow": allow},
                )
            time.sleep(0.2)
    except KeyboardInterrupt:
        request("POST", f"/v1/sessions/{session}/cancel", {})
        request("POST", f"/v1/sessions/{session}/close", {})
        return 130


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print("error:", error, file=sys.stderr)
        sys.exit(1)
