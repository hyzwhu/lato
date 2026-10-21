#!/usr/bin/env python3
"""Stdlib-only MCP example: two real modules over synthetic customer data."""

import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent
MAX_LINE_BYTES = 256 * 1024
TOOLS = [
    {
        "name": "lookup_customer",
        "description": "Look up a customer by name or ID in the synthetic demo dataset.",
        "inputSchema": {
            "type": "object",
            "properties": {"customer": {"type": "string"}},
            "required": ["customer"],
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True, "idempotentHint": True},
    },
    {
        "name": "create_customer_report",
        "description": "Create a Markdown report for a customer ID returned by lookup_customer. Writes a demo artifact under this module's outputs directory.",
        "inputSchema": {
            "type": "object",
            "properties": {"customerId": {"type": "string", "enum": ["acme", "northwind"]}},
            "required": ["customerId"],
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": False, "idempotentHint": True},
    },
]


def invoke(name, arguments):
    dataset = json.loads((ROOT / "customers.json").read_text(encoding="utf-8"))
    customers = dataset["customers"]
    if name == "lookup_customer":
        if set(arguments) != {"customer"} or not isinstance(arguments["customer"], str):
            raise ValueError("customer must be a string and the only argument")
        query = arguments["customer"].strip().casefold()
        customer = next(
            (item for item in customers.values() if query in (item["id"].casefold(), item["name"].casefold())),
            None,
        )
        if customer is None:
            raise ValueError("customer not found; available demo customers: Acme, Northwind")
        return {"customer": customer, "dataSource": dataset["source"]}
    if name == "create_customer_report":
        if set(arguments) != {"customerId"} or arguments["customerId"] not in ("acme", "northwind"):
            raise ValueError("customerId must be acme or northwind")
        customer = customers[arguments["customerId"]]
        report = (
            f"# {customer['name']} customer report\n\n"
            f"Period: {customer['period']}\n\n"
            f"Revenue: {customer['revenue']} {customer['currency']}\n\n"
            f"Orders: {customer['orders']}\n\n"
            f"Data source: {dataset['source']}\n"
        )
        outputs = ROOT / "outputs"
        outputs.mkdir(exist_ok=True)
        path = outputs / (customer["id"] + "-report.md")
        path.write_text(report, encoding="utf-8")
        return {"path": str(path), "bytes": path.stat().st_size, "customer": customer, "dataSource": dataset["source"]}
    raise ValueError("unknown business tool")


def reply(identifier, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": identifier}
    message["error" if error else "result"] = error if error else result
    print(json.dumps(message, ensure_ascii=False), flush=True)


def main():
    while True:
        line = sys.stdin.buffer.readline(MAX_LINE_BYTES + 1)
        if not line:
            return
        if len(line) > MAX_LINE_BYTES:
            # Do not parse a truncated request or treat its remainder as a new one.
            return
        try:
            message = json.loads(line)
        except (ValueError, UnicodeError):
            reply(None, error={"code": -32700, "message": "invalid JSON"})
            continue
        if not isinstance(message, dict):
            reply(None, error={"code": -32600, "message": "request must be an object"})
            continue
        if "id" not in message:
            continue
        identifier = message["id"]
        method = message.get("method")
        if method == "initialize":
            reply(identifier, {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                               "serverInfo": {"name": "business-demo", "version": "0.1.0"}})
        elif method == "ping":
            reply(identifier, {})
        elif method == "tools/list":
            reply(identifier, {"tools": TOOLS})
        elif method == "tools/call":
            params = message.get("params")
            try:
                if not isinstance(params, dict) or not isinstance(params.get("arguments", {}), dict):
                    raise ValueError("tool call parameters must contain an arguments object")
                output = invoke(params.get("name"), params.get("arguments", {}))
                reply(identifier, {"content": [{"type": "text", "text": json.dumps(output, ensure_ascii=False)}], "isError": False})
            except (OSError, ValueError, KeyError, TypeError):
                reply(identifier, {"content": [{"type": "text", "text": "Business operation failed: check tool name, customer ID, and argument schema."}], "isError": True})
        else:
            reply(identifier, error={"code": -32601, "message": "method not found"})


if __name__ == "__main__":
    main()
