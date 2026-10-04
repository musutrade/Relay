"""Real server signal/restart tests; fake local commands only, no model calls."""
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get("RELAY_APP_BINARY", ROOT / "target/debug/relay-app"))
CORE = Path(os.environ.get("RELAY_BINARY", ROOT / "target/debug/relay"))
TOKEN = "offline-shutdown-test-token-00000000"


class ShutdownTest(unittest.TestCase):
    def start(self, root):
        process = subprocess.Popen(
            [str(BINARY), "serve", str(root / "config.json"), str(root / "relay.db"), "127.0.0.1:0"],
            env={**os.environ, "RELAY_TOKEN": TOKEN}, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.cleanup, process)
        self.assertTrue(select.select([process.stderr], [], [], 10)[0], "server did not start")
        line = process.stderr.readline()
        self.assertIn("Relay listening at http://", line)
        return process, "http://" + line.split("http://", 1)[1].split(" ", 1)[0]

    @staticmethod
    def cleanup(process):
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process.stderr.close()

    @staticmethod
    def request(base, path, payload=None):
        body = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(base + path, body, {
            "Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=3) as response:
            return json.load(response)

    def wait_for(self, read, predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            value = read()
            if predicate(value):
                return value
            time.sleep(.03)
        self.fail("condition not reached: " + repr(value))

    @staticmethod
    def config(root):
        config = json.loads(subprocess.check_output([
            sys.executable, str(ROOT / "examples/make-demo-config.py"), str(root / "workspaces")]))
        fake = root / "long.py"
        fake.write_text("import os, pathlib, subprocess, sys, time\n"
                        "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])\n"
                        "pathlib.Path(os.environ['PID_FILE']).write_text(f'{os.getpid()} {child.pid}')\n"
                        "time.sleep(120)\n")
        config["agents"]["long"] = {"program": sys.executable, "args": [str(fake)],
                                      "env": {"PID_FILE": str(root / "pids")}}
        config["timeout_seconds"] = 60
        (root / "config.json").write_text(json.dumps(config))

    def test_signals_stop_process_tree_persist_result_and_restart(self):
        for sig in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=sig), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.config(root)
                process, base = self.start(root)
                task = self.request(base, "/api/tasks", {"key": "long", "job": {
                    "repository": "demo", "requirements": "Wait for shutdown", "agent": "long", "publish": False}})
                self.wait_for(lambda: (root / "pids").exists(), bool)
                pids = self.wait_for(lambda: (root / "pids").read_text().split(), lambda p: len(p) == 2)
                process.send_signal(sig)
                self.assertEqual(process.wait(timeout=10), 0)
                for pid in pids:
                    self.assertFalse(Path("/proc", pid).exists(), "child still exists after server exit")
                process, base = self.start(root)
                old = self.request(base, "/api/tasks/" + str(task["id"]))
                self.assertEqual(old["state"], "finished")
                self.assertEqual(json.loads(old["result"])["outcome"], "cancelled")
                following = self.request(base, "/api/tasks", {"key": "next", "job": {
                    "repository": "demo", "requirements": "Create artifact", "agent": "fake", "test": "demo", "publish": False}})
                result = self.wait_for(lambda: self.request(base, "/api/tasks/" + str(following["id"])),
                                       lambda task: task["state"] == "finished")
                self.assertEqual(json.loads(result["result"])["outcome"], "success")
                process.send_signal(sig)
                self.assertEqual(process.wait(timeout=10), 0)

    def test_shutdown_does_not_release_an_unknown_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.config(root)
            payload = root / "job.json"
            payload.write_text(json.dumps({"repository": "demo", "requirements": "Unknown execution", "agent": "fake"}))
            def core(*args):
                return json.loads(subprocess.check_output([str(CORE), str(root / "relay.db"), *args]))
            core("submit", "unknown", str(payload))
            claimed = core("claim", "unobserved-host")
            for sig in (signal.SIGTERM, signal.SIGINT):
                process, base = self.start(root)
                self.assertTrue(self.request(base, "/api/status")["recovery_required"])
                process.send_signal(sig)
                self.assertEqual(process.wait(timeout=10), 0)
                self.assertEqual(core("get", str(claimed["id"])), claimed)


if __name__ == "__main__":
    unittest.main()
