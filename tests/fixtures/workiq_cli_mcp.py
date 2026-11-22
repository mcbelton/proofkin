"""A fake `workiq mcp` (stdio, newline-delimited JSON-RPC) for tests/workiq_cli.rs.

Usage: workiq_cli_mcp.py <state dir> <mode> [--tenant-id <id>]

Modes:
  ok            answers everything
  user          like ok, and says who is signed in in the initialize reply
  crash         exits without answering on the first tools/call of the whole test (a marker
                file in the state dir says it did)
  crash-always  exits without answering on every tools/call
  eula          says on stderr that the EULA is not accepted, and exits at once

Before every response it sends a notification and a line that is not JSON, which a client
has to skip. Every start and every message's method is appended to <state dir>/log.txt.
Tools: `retrieve` (echoes the query), `send_mail` (a change tool), `inspect` (reports the
command line and which of the environment variables named in `names` are set).
"""

import json
import os
import sys

STATE_DIR, MODE = sys.argv[1], sys.argv[2]
ARGV = sys.argv[3:]


def log(text):
    with open(os.path.join(STATE_DIR, "log.txt"), "a", encoding="utf-8") as out:
        out.write(text + "\n")


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


log("start")

if MODE == "eula":
    sys.stderr.write("The Work IQ EULA is not accepted. Run: workiq accept-eula\n")
    sys.stderr.flush()
    sys.exit(1)

TOOLS = [
    {"name": name, "description": name, "inputSchema": {"type": "object", "properties": {}}}
    for name in ("retrieve", "send_mail", "inspect")
]


def text_result(text):
    return {"content": [{"type": "text", "text": text}]}


for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method, mid = msg.get("method"), msg.get("id")
    log(method)
    if mid is None:
        continue
    params = msg.get("params") or {}
    if method == "tools/call":
        marker = os.path.join(STATE_DIR, "crashed")
        if MODE == "crash-always" or (MODE == "crash" and not os.path.exists(marker)):
            open(marker, "w").close()
            sys.exit(1)
    send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"data": "hello"}})
    sys.stdout.write("this line is not JSON\n")
    if method == "initialize":
        info = {"name": "workiq", "version": "1.0.0"}
        if MODE == "user":
            info["user"] = "alice@contoso.example"
        result = {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": info,
        }
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        arguments = params.get("arguments") or {}
        if params.get("name") == "inspect":
            names = arguments.get("names", [])
            result = text_result(
                json.dumps({"argv": ARGV, "set": [n for n in names if n in os.environ]})
            )
        else:
            result = text_result("%s: %s" % (params.get("name"), arguments.get("query", "")))
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no method"}})
        continue
    send({"jsonrpc": "2.0", "id": mid, "result": result})
