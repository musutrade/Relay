"""Real Chromium + real Relay HTTP test; only temporary fixture credentials.
CI runs this after cargo build. No production credentials, CLI providers or TLS bypass.
Secure cookie attributes/HTTPS Origin enforcement are covered by Rust HTTP tests;
this explicitly enabled HTTP-loopback browser fixture exercises real cookie lifetime.
"""
import json
import os
import pty
import select
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / 'target/debug/relay-app'
PASSWORD = 'fixture password only 123!'

def initialize(path, rotate=False):
    pid, fd = pty.fork()
    if pid == 0:
        args = [str(BINARY), 'auth-password' if rotate else 'auth-init', str(path)]
        if not rotate:
            args.append('operator')
        os.execv(str(BINARY), args)
    transcript = b''
    deadline = time.monotonic() + 15
    sent = 0
    try:
        while time.monotonic() < deadline:
            if select.select([fd], [], [], .2)[0]:
                try:
                    transcript += os.read(fd, 4096)
                except OSError:
                    break
                prompts = [b'New Relay password', b'Confirm password:']
                if sent < 2 and prompts[sent] in transcript:
                    os.write(fd, (PASSWORD + '\n').encode())
                    sent += 1
        while time.monotonic() < deadline:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                break
            time.sleep(.05)
        else:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            raise AssertionError('Credential initializer exceeded 15 seconds')
        assert os.waitstatus_to_exitcode(status) == 0, transcript.decode()
        assert PASSWORD.encode() not in transcript, 'Terminal echoed password'
    finally:
        os.close(fd)

with tempfile.TemporaryDirectory(prefix='relay-auth-browser-') as temporary:
    root = Path(temporary)
    repo = root / 'source'; repo.mkdir(); (repo / 'README.md').write_text('fixture')
    credentials = root / 'credentials.json'; initialize(credentials)
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0)); port = probe.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    config = root / 'config.json'
    config.write_text(json.dumps({'workspace_root':str(root / 'runs'), 'repositories':{'fixture':str(repo)}, 'agents':{'fake':{'program':'/bin/true'}}}))
    auth = root / 'auth.json'
    auth.write_text(json.dumps({'mode':'session','credentials_file':str(credentials),'public_origin':base,'allow_insecure_loopback':True}))
    env = dict(os.environ, RELAY_AUTH_CONFIG=str(auth)); env.pop('RELAY_TOKEN', None)
    server = subprocess.Popen([str(BINARY), 'serve', str(config), str(root / 'relay.db'), f'127.0.0.1:{port}'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'), headless=True, args=['--no-sandbox'])
            context = browser.new_context()
            page = context.new_page(); errors = []; page.on('pageerror', lambda e: errors.append(str(e)))
            for _ in range(100):
                try:
                    page.goto(base); break
                except Exception:
                    if server.poll() is not None: raise RuntimeError(server.stderr.read().decode())
                    time.sleep(.05)
            page.locator('#username').wait_for()
            assert context.request.get(base + '/api/config').status == 401
            page.locator('#username').fill('operator'); page.locator('#password').fill('wrong')
            page.locator('#connect').click(); page.locator('#auth-error').wait_for(state='visible')
            assert page.locator('#password').input_value() == ''
            page.locator('#password').fill(PASSWORD); page.locator('#connect').click()
            page.locator('#auth-panel').wait_for(state='hidden')
            assert page.locator('#password').input_value() == ''
            cookie = context.cookies()[0]
            assert cookie['httpOnly'] and cookie['sameSite'] == 'Strict'
            assert cookie['expires'] > time.time() + 6 * 86400
            assert page.evaluate('localStorage.length + sessionStorage.length') == 0
            page.reload(); page.locator('#auth-panel').wait_for(state='hidden')
            assert context.request.post(base + '/api/tasks/1/cancel', headers={'Origin':'https://evil.example'}).status == 401
            assert context.request.post(base + '/auth/logout', headers={'Origin':'https://evil.example'}).status == 403
            assert context.request.post(base + '/auth/login', headers={'Origin':base, 'Content-Type':'application/json'}, data=json.dumps({'username':'operator', 'password':'x' * 5000})).status == 413
            saved = context.storage_state()
            other = browser.new_context(storage_state=saved)
            reopened = other.new_page(); reopened.goto(base); reopened.locator('#auth-panel').wait_for(state='hidden')
            page.locator('#logout').click(); page.locator('#auth-panel').wait_for(state='visible')
            assert context.request.get(base + '/api/config').status == 401
            assert other.request.get(base + '/api/config').status == 401
            # Authenticate again, then rotate locally; copied old cookies must fail.
            page.locator('#username').fill('operator'); page.locator('#password').fill(PASSWORD); page.locator('#connect').click()
            page.locator('#auth-panel').wait_for(state='hidden')
            initialize(credentials, rotate=True)
            assert context.request.get(base + '/api/config').status == 401
            assert not errors, errors
            other.close(); browser.close()
        print('Real Relay browser authentication: invalid/valid login, reload/reopen, logout, Origin rejection and credential rotation passed')
    finally:
        server.terminate()
        try: server.communicate(timeout=10)
        except subprocess.TimeoutExpired: server.kill(); server.communicate()
