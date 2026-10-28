"""Run: python3 -m unittest discover -s scripts -p test_mcp_local_bridge.py"""
import importlib.util
import io
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location("bridge", Path(__file__).with_name("mcp-local-bridge.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.token = Path(self.temp.name) / "api.token"
        self.token.write_text("private-test-token\n")
        self.token.chmod(0o600)
        self.requests = []
        self.status = 200
        self.invalid = False
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                owner.requests.append((dict(self.headers), message))
                reply = {"jsonrpc": "2.0", "id": message.get("id"), "result": {"isError": True, "content": []}}
                self.send_response(owner.status)
                self.send_header("Location", "http://example.com/never-follow")
                self.end_headers()
                self.wfile.write(b"invalid" if owner.invalid else json.dumps(reply).encode())

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.client = bridge.Bridge("http://127.0.0.1:%d/mcp" % self.server.server_port, self.token)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temp.cleanup()

    def test_dynamic_headers_and_tool_errors_survive(self):
        for i, (method, params) in enumerate([
            ("initialize", {}), ("tools/list", {}),
            ("tools/call", {"name": "profile.whoami"}),
            ("tools/call", {"name": "memory.recall"}),
        ]):
            reply = self.client.request({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
            headers, message = self.requests[-1]
            self.assertEqual(headers["Mcp-Method"], method)
            self.assertEqual(headers.get("Mcp-Name"), params.get("name"))
            self.assertEqual(headers["Authorization"], "Bearer private-test-token")
            self.assertEqual(reply["id"], i)
            self.assertTrue(reply["result"]["isError"])

    def test_notifications_have_no_stdout_and_invalid_json_recovers(self):
        output = io.StringIO()
        bridge.serve(self.client, io.BytesIO(b'bad\n{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
                                            b'{"jsonrpc":"2.0","id":"ok","method":"ping"}\n'), output)
        replies = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(len(replies), 2)
        self.assertEqual(replies[0]["error"]["code"], -32700)
        self.assertEqual(replies[1]["id"], "ok")

    def test_private_token_and_rotation(self):
        self.token.chmod(0o644)
        with self.assertRaises(ValueError):
            self.client.token()
        self.token.chmod(0o600)
        self.token.write_text("rotated")
        self.assertEqual(self.client.token(), "rotated")

    def test_refuses_remote_urls_and_embedded_credentials(self):
        for url in ["https://example.com/mcp", "http://localhost/mcp", "http://127.0.0.1/mcp?token=x",
                    "http://user:secret@127.0.0.1/mcp", "http://127.0.0.1/other"]:
            with self.assertRaises(ValueError):
                bridge.Bridge(url, self.token)

    def test_http_failures_are_errors_not_success_or_redirects(self):
        for status in [401, 302, 404]:
            self.status = status
            reply = self.client.request({"jsonrpc": "2.0", "id": 7, "method": "ping"})
            self.assertIn("error", reply)
            self.assertNotIn("private-test-token", json.dumps(reply))
        self.assertEqual(len(self.requests), 3)
        self.status = 200
        self.invalid = True
        self.assertIn("error", self.client.request({"jsonrpc": "2.0", "id": 8, "method": "ping"}))

    def test_configs_contain_absolute_paths_but_no_token(self):
        for name in ["claude-code", "codex", "cursor", "claude-desktop"]:
            result = bridge.configuration(name, self.client)
            self.assertIn(str(self.token), result)
            self.assertIn(str(Path(bridge.__file__).resolve()), result)
            self.assertNotIn("private-test-token", result)


if __name__ == "__main__":
    unittest.main()
