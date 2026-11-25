"""A fake Work IQ CLI (`workiq mcp`: stdio, newline-delimited JSON-RPC) for tests/workiq_cli.rs
and tests/workiq_e2e.rs.

Usage: workiq_cli_mcp.py <state dir> <mode> mcp [--account <email>]
       workiq_cli_mcp.py <state dir> <mode> config show

Like the real CLI (1.0.1), `mcp` takes `--account`, `--endpoint` and `--log-level` and refuses
any other option (`--tenant-id` among them), and `config show` prints `key=value` lines with
the default account (none in mode `bare`).

Modes:
  ok            answers everything
  full          like ok, but offers the tools Proofkin's commands use, answering as the
                content policy reads them: `retrieve` (one General file and one blocked one in
                structuredContent), `ask` (an answer citing a General file), `list_agents`
                (one agent)
  bare          like ok, but `config show` names no default account
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


if ARGV[:2] == ["config", "show"]:
    log("config show")
    if MODE != "bare":
        print("defaultAccount=bob@contoso.example")
    print("isMSITTenant=false")
    sys.exit(0)

if ARGV[:1] != ["mcp"]:
    sys.stderr.write("Unrecognized command or argument '%s'.\n" % " ".join(ARGV))
    sys.exit(1)
OPTIONS = ARGV[1:]
while OPTIONS:
    if OPTIONS[0] not in ("--account", "--endpoint", "-e", "--log-level", "-l") or len(OPTIONS) < 2:
        sys.stderr.write("Unrecognized command or argument '%s'.\n" % OPTIONS[0])
        sys.exit(1)
    OPTIONS = OPTIONS[2:]

log("start")

if MODE == "eula":
    sys.stderr.write("The Work IQ EULA is not accepted. Run: workiq accept-eula\n")
    sys.stderr.flush()
    sys.exit(1)

TOOLS = [
    {"name": name, "description": name, "inputSchema": {"type": "object", "properties": {}}}
    for name in (
        ("retrieve", "ask", "list_agents") if MODE == "full" else ("retrieve", "send_mail", "inspect")
    )
]


def text_result(text):
    return {"content": [{"type": "text", "text": text}]}


def cli_file(fid, protection, text):
    return {
        "id": fid,
        "markdown": text,
        "webUrl": "https://contoso.example/files/%s" % fid,
        "entityType": "file",
        "contentProtection": protection,
        "sensitivityLabel": {"displayName": "General"},
        "resourceMetadata": {"document": {"title": "CLI file %s" % fid}},
        "relevanceScore": 0.9,
    }


def full_result(name, arguments):
    """What the `full` mode answers: the text, and the structuredContent Proofkin reads."""
    if name == "retrieve":
        files = [
            cli_file("c1", "allowed", "Launch budget notes from the CLI."),
            cli_file("c2", "blocked", "A file the CLI may not hand out."),
        ]
        structured = {
            "application/vnd.ms-workiq.retrieval": {
                "results": files,
                "resultCount": len(files),
                "stoppedReason": "completed",
            }
        }
    elif name == "ask":
        structured = {
            "answer": "The CLI says the launch moved to May.",
            "citations": [{"title": "Launch plan", "sensitivityLabel": {"displayName": "General"}}],
        }
    else:
        structured = {"agents": [{"id": "sales:1", "name": "Sales"}]}
    result = text_result("%s: %s" % (name, arguments.get("query", "")))
    result["structuredContent"] = structured
    return result


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
        if MODE == "full":
            result = full_result(params.get("name"), arguments)
        elif params.get("name") == "inspect":
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
