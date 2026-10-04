"""Actual socket/service demo using only bundled fake adapters; run after cargo test."""
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
TOKEN = "offline-smoke-token-000000000000000000"


class ServiceSmoke(unittest.TestCase):
    def test_bundled_demo_over_authenticated_http(self):
        binary = pathlib.Path(os.environ.get("RELAY_APP_BINARY", str(ROOT / "target/debug/relay-app")))
        self.assertTrue(binary.is_file(), "build relay-app with cargo test first")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            config = subprocess.check_output([sys.executable, str(ROOT / "examples/make-demo-config.py"), str(root / "workspaces")])
            (root / "config.json").write_bytes(config)
            process = subprocess.Popen([str(binary), "serve", str(root / "config.json"), str(root / "relay.db"), "127.0.0.1:0"], env={**os.environ, "RELAY_TOKEN": TOKEN}, stderr=subprocess.PIPE, text=True)
            try:
                line = process.stderr.readline()
                self.assertIn("Relay listening at http://", line)
                base = line.split("http://", 1)[1].split(" ", 1)[0]
                base = "http://" + base
                with urllib.request.urlopen(base + "/", timeout=3) as response:
                    self.assertEqual(response.status, 200)
                    self.assertEqual(response.headers["Cache-Control"], "no-store")
                    self.assertIn(b"<html", response.read())
                with self.assertRaises(urllib.error.HTTPError) as unauthorized:
                    urllib.request.urlopen(base + "/api/tasks", timeout=3)
                self.assertEqual(unauthorized.exception.code, 401)
                headers = {"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"}
                payload = {"key": "demo-smoke", "job": {"repository": "demo", "requirements": "Create and verify the demo artifact", "agent": "fake", "test": "demo", "publish": True, "draft_pr_adapter": "mock"}}
                request = urllib.request.Request(base + "/api/tasks", json.dumps(payload).encode(), headers)
                with urllib.request.urlopen(request, timeout=3) as response:
                    task = json.load(response)
                deadline = time.monotonic() + 10
                while task["state"] != "finished" and time.monotonic() < deadline:
                    time.sleep(.05)
                    with urllib.request.urlopen(urllib.request.Request(base + "/api/tasks/" + str(task["id"]), headers=headers), timeout=3) as response:
                        task = json.load(response)
                self.assertEqual(task["state"], "finished", task)
                result = json.loads(task["result"])
                self.assertEqual(result["outcome"], "success", result)
                self.assertIn("Demo acceptance test passed", result["tests"]["stdout"])
                self.assertTrue(json.loads(result["draft_pr"]["stdout"])["dry_run"])
            finally:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                process.stderr.close()
            self.assertEqual(process.returncode, 0)


if __name__ == "__main__":
    unittest.main()
