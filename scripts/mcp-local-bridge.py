#!/usr/bin/env python3
"""Source-only stdio client for the authenticated local ShogunAgent HTTP server.

No packages, model calls, token copies, or changes to client configuration. See
docs/local-agent-setup.md. stdout is exclusively newline-delimited JSON-RPC.
"""

import argparse
import http.client
import json
import os
from pathlib import Path
import stat
import sys
from urllib.parse import urlsplit

MAX_BYTES = 8 * 1024 * 1024


def rpc_error(request_id, message, code=-32000):
    return {"jsonrpc": "2.0", "id": request_id,
            "error": {"code": code, "message": message}}


class Bridge:
    def __init__(self, url, token_file, timeout=30):
        parsed = urlsplit(url)
        if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1"
                or parsed.username or parsed.password or parsed.query
                or parsed.fragment or parsed.path != "/mcp"):
            raise ValueError("Use http://127.0.0.1:<port>/mcp for the local server.")
        self.port = parsed.port or 80
        self.timeout = timeout
        self.token_file = Path(token_file).expanduser().resolve()

    def token(self):
        # Read again per request, so rotating a token does not require restarting clients.
        with self.token_file.open() as stream:
            mode = os.fstat(stream.fileno()).st_mode
            if not stat.S_ISREG(mode) or (os.name == "posix" and mode & 0o077):
                raise ValueError("The token must be a private regular file (chmod 600).")
            token = stream.read(4097).strip()
        if not token or len(token) > 4096 or any(c.isspace() for c in token):
            raise ValueError("The token file must contain one nonempty bearer token.")
        return token

    def request(self, message):
        if (not isinstance(message, dict) or message.get("jsonrpc") != "2.0"
                or not isinstance(message.get("method"), str)):
            return rpc_error(None, "Expected one JSON-RPC 2.0 request.", -32600)
        request_id = message.get("id")
        notification = "id" not in message
        method = message["method"]
        headers = {"Content-Type": "application/json", "Accept": "application/json",
                   "Mcp-Method": method}
        if method == "tools/call":
            params = message.get("params")
            name = params.get("name") if isinstance(params, dict) else None
            if not isinstance(name, str) or not name:
                return None if notification else rpc_error(request_id, "tools/call requires params.name.", -32602)
            headers["Mcp-Name"] = name
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=self.timeout)
        try:
            headers["Authorization"] = "Bearer " + self.token()
            body = json.dumps(message).encode()
            if len(body) > MAX_BYTES:
                raise ValueError("Request exceeds the 8 MiB limit.")
            # HTTPConnection uses no proxy environment and never follows redirects.
            connection.request("POST", "/mcp", body, headers)
            response = connection.getresponse()
            raw = response.read(MAX_BYTES + 1)
            if len(raw) > MAX_BYTES:
                raise ValueError("Response exceeds the 8 MiB limit.")
            if notification:
                if response.status not in (200, 202, 204):
                    print("ShogunAgent notification refused (HTTP %d)." % response.status, file=sys.stderr)
                return None
            if response.status == 401:
                return rpc_error(request_id, "ShogunAgent rejected the token (HTTP 401). Check the server and token file.")
            if response.status not in (200, 400, 403, 429, 500, 503):
                return rpc_error(request_id, "ShogunAgent HTTP %d; redirects are not followed." % response.status)
            reply = json.loads(raw)
            if (not isinstance(reply, dict) or reply.get("jsonrpc") != "2.0"
                    or reply.get("id") != request_id
                    or ("result" in reply) == ("error" in reply)):
                raise ValueError("Server returned an invalid JSON-RPC response.")
            return reply
        except (OSError, ValueError, http.client.HTTPException):
            # Never expose HTTP exception strings: they may contain a bearer or source text.
            if notification:
                print("ShogunAgent notification failed; check the local server and private token file.", file=sys.stderr)
                return None
            return rpc_error(request_id, "Cannot reach ShogunAgent or read its response. Check the local server, port, and private token file.")
        finally:
            connection.close()


def serve(bridge, input_stream, output_stream):
    while True:
        line = input_stream.readline(MAX_BYTES + 1)
        if not line:
            return
        if len(line) > MAX_BYTES:
            output_stream.write(json.dumps(rpc_error(None, "Request exceeds the 8 MiB limit.", -32600)) + "\n")
            output_stream.flush()
            return
        if not line.strip():
            continue
        try:
            message = json.loads(line)
        except (ValueError, UnicodeError):
            reply = rpc_error(None, "Invalid JSON.", -32700)
        else:
            reply = bridge.request(message)
        if reply is not None:
            output_stream.write(json.dumps(reply, ensure_ascii=True) + "\n")
            output_stream.flush()


def configuration(client, bridge):
    entry = {"command": sys.executable, "args": [str(Path(__file__).resolve()),
             "--url", "http://127.0.0.1:%d/mcp" % bridge.port,
             "--token-file", str(bridge.token_file)]}
    if client == "codex":
        return "[mcp_servers.shogunagent]\ncommand = %s\nargs = %s\n" % (
            json.dumps(entry["command"]), json.dumps(entry["args"]))
    return json.dumps({"mcpServers": {"shogunagent": entry}}, indent=2) + "\n"


def check(bridge):
    calls = [
        ("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                        "clientInfo": {"name": "shogun-connection-check", "version": "1"}}),
        ("tools/list", {}),
        ("tools/call", {"name": "profile.whoami", "arguments": {}}),
    ]
    for index, (method, params) in enumerate(calls, 1):
        reply = bridge.request({"jsonrpc": "2.0", "id": index, "method": method, "params": params})
        if "error" in reply or reply.get("result", {}).get("isError"):
            print("FAIL: " + method + "; check the local server, token, and permissions.", file=sys.stderr)
            return 1
        print("PASS: " + method)
        if method == "initialize":
            bridge.request({"jsonrpc": "2.0", "method": "notifications/initialized"})
    print("Local protocol check passed. This is not an installed-client or model test.")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:7464/mcp")
    parser.add_argument("--token-file", required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--print-config", choices=["claude-code", "cursor", "claude-desktop", "codex"])
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()
    try:
        bridge = Bridge(args.url, args.token_file)
        if args.print_config:
            print(configuration(args.print_config, bridge), end="")
            return 0
        bridge.token()
    except (OSError, ValueError):
        parser.error("Use a loopback /mcp URL and a readable private token file (chmod 600).")
    if args.check:
        return check(bridge)
    serve(bridge, sys.stdin.buffer, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
