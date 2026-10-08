"""Desktop/mobile CI tracking fixture. APIs are intercepted; no GitHub/model calls.
Run in the existing Playwright CI browser job, not on socket-restricted hosts.
"""
import copy
import json
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

HTML = (Path(__file__).resolve().parents[1] / 'static' / 'index.html').read_bytes()
SCREENSHOTS = Path(os.environ.get('RELAY_UI_SCREENSHOTS') or tempfile.mkdtemp(prefix='relay-ci-'))
SCREENSHOTS.mkdir(parents=True, exist_ok=True)


class Server(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(HTML)

    def log_message(self, *_args):
        pass


def task(task_id):
    return {'id': task_id, 'key': f'task-{task_id}', 'payload': json.dumps({
        'repository': 'fixture', 'requirements': 'Track the published exact HEAD', 'agent': 'fake',
        'test': 'test', 'publish': False}), 'state': 'finished' if task_id == 1 else 'queued',
        'generation': 1 if task_id == 1 else 0, 'owner': 'fixture' if task_id == 1 else None,
        'result': json.dumps({'outcome': 'success', 'workspace': '/fixture'}) if task_id == 1 else None,
        'continuation_status': None}


def operator(value):
    return {'task_id': value['id'], 'generation': value['generation'], 'failure': None,
        'resources': {'usage': {'logical_bytes': 500, 'complete': True, 'measured_at': 1700000000, 'reason': None},
            'quota_bytes': 1000, 'host_policy_cap_bytes': 1000, 'snapshot_cap_bytes': 1000},
        'retained_result': {'available': value['result'] is not None, 'immutable': True},
        'workspace_retained': True, 'recovery': {'inherited_quota_bytes': 1000,
            'actions': [], 'blocked_reason': None, 'successor_id': None, 'reserved_request': None}}


def preview():
    return {'eligible': True, 'reason': None, 'publication': {
        'repository': 'fixture/project', 'base_branch': 'main', 'head_sha': 'a' * 40,
        'head_branch': 'relay/task-1-g1', 'pr_number': 19, 'pr_url': 'https://github.com/fixture/project/pull/19'},
        'policies': [{'name': 'required-ci', 'policy_digest': 'd' * 64, 'workflow_id': 41, 'app_id': 15368,
            'event': 'pull_request', 'required_jobs': ['check', 'browser'], 'poll_interval_seconds': 60,
            'observation_window_seconds': 3600}], 'tracks': [], 'remote_merge_eligibility': 'not_established'}


def track(ci):
    now = int(time.time())
    return dict(ci['publication'], **ci['policies'][0], id=10, publication_task_id=1, policy='required-ci',
        status='watching', attempt=0, revision=1, stop_requested=False, created_at=now, deadline=now + 3600,
        next_poll_at=now + 60, last_observed_at=None, window_generation=1, window_started_at=now,
        observed_repository_id=None, observed_pr_id=None, observed_base_sha=None, latest_evidence=None,
        diagnostic=None, remote_merge_eligibility='not_established')


server = ThreadingHTTPServer(('127.0.0.1', 0), Server)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'), headless=True, args=['--no-sandbox'])
        for width, height in [(1440, 1150), (390, 844)]:
            context = browser.new_context(viewport={'width': width, 'height': height}, locale='zh-CN')
            page = context.new_page()
            errors, posts = [], []
            data = {'ci': preview(), 'mode': 'lost'}
            page.on('pageerror', lambda error: errors.append(str(error)))

            def route(intercept):
                req = intercept.request
                path = req.url.removeprefix(base)
                if path == '/auth/status':
                    response = {'mode': 'bearer', 'authenticated': False}
                else:
                    assert req.headers.get('authorization') == 'Bearer fixture-token'
                    if path == '/api/config':
                        response = {'repositories': ['fixture'], 'agents': ['fake'], 'tests': ['test']}
                    elif path == '/api/status':
                        response = {'active': None, 'recovery_required': False, 'diagnostic': None}
                    elif path == '/api/tasks':
                        response = [task(2), task(1)]
                    elif path.endswith('/operator'):
                        response = operator(task(int(path.split('/')[-2])))
                    elif path.endswith('/ci-preview'):
                        response = data['ci']
                    elif req.method == 'POST':
                        body = req.post_data_json
                        posts.append((path, copy.deepcopy(body)))
                        if path == '/api/tasks/1/track-ci':
                            assert body == {'key': body['key'], 'policy': 'required-ci', 'policy_digest': 'd' * 64}
                            if data['mode'] == 'lost':
                                intercept.abort('failed')
                                return
                            data['ci']['tracks'] = [track(data['ci'])]
                        else:
                            current = data['ci']['tracks'][0]
                            assert body == {'expected_revision': current['revision']}
                            current['revision'] += 1
                            if path.endswith('/stop'):
                                current.update(status='stopped', stop_requested=True)
                            else:
                                assert path.endswith('/resume')
                                now = int(time.time())
                                current.update(status='watching', stop_requested=False,
                                    window_generation=current['window_generation'] + 1,
                                    window_started_at=now, deadline=now + 3600)
                        response = data['ci']['tracks'][0]
                    else:
                        response = task(int(path.split('/')[-1]))
                intercept.fulfill(status=200, content_type='application/json', body=json.dumps(response))

            page.route('**/api/**', route)
            page.route('**/auth/**', route)
            page.goto(base)
            page.locator('#token').fill('fixture-token')
            page.locator('#auth-form button[type=submit]').click()
            page.locator('[data-task-id="1"]').click()
            expect(page.locator('#ci-start')).to_be_enabled()
            expect(page.locator('#ci-policy')).to_have_value('required-ci')
            expect(page.locator('#ci-scope')).to_contain_text('a' * 40)
            expect(page.locator('#ci-scope')).to_contain_text('fixture/project')
            expect(page.locator('#ci-policy-scope')).to_contain_text('App ID：15368')
            assert not posts
            assert page.locator('#detail-ci input').count() == 0
            page.locator('#ci-start').click()
            expect(page.locator('#ci-error')).to_contain_text('尚未确认')
            assert len(posts) == 1
            first = copy.deepcopy(posts[0])
            data['mode'] = 'success'
            page.locator('#ci-start').click()
            expect(page.locator('#ci-tracks')).to_contain_text('正在跟踪')
            assert posts == [first, first]
            expect(page.locator('#ci-start')).not_to_be_visible()
            expect(page.locator('#ci-tracks')).to_contain_text('当前窗口固定截止时间')
            assert page.locator('#detail-ci').evaluate('(e) => e.scrollWidth <= e.clientWidth')
            page.screenshot(path=str(SCREENSHOTS / f'ci-watching-{width}.png'), full_page=True)
            page.locator('[data-ci-action="stop"]').click()
            expect(page.locator('#ci-tracks')).to_contain_text('已停止')
            page.locator('[data-ci-action="resume"]').click()
            expect(page.locator('#ci-tracks')).to_contain_text('观察窗口 2')
            current = data['ci']['tracks'][0]
            current.update(status='checks_failed', revision=current['revision'] + 1, last_observed_at=int(time.time()),
                latest_evidence={'version': 1, 'observation': 'ok', 'complete': True,
                    'remote_merge_eligibility': 'not_established', 'observed_at': int(time.time()),
                    'missing_jobs': ['browser'], 'failed_jobs': ['check'], 'pending_jobs': [],
                    'run': {'id': 501, 'run_attempt': 2, 'jobs': [{'name': 'check', 'status': 'completed', 'conclusion': 'failure'}]}})
            page.locator('#ci-read').click()
            expect(page.locator('#ci-tracks')).to_contain_text('失败检查：check')
            expect(page.locator('#ci-tracks')).to_contain_text('缺失检查：browser')
            current.update(status='configured_checks_passed', revision=current['revision'] + 1)
            current['latest_evidence'].update(missing_jobs=[], failed_jobs=[])
            page.locator('#ci-read').click()
            expect(page.locator('.ci-result')).to_have_text('configured checks passed; remote merge eligibility not established')
            expect(page.locator('.ci-boundary')).to_contain_text('Draft PR 保持 draft')
            current.update(status='process_unknown', revision=current['revision'] + 1,
                diagnostic={'code': 'ci_cleanup_unknown', 'message': 'Inspect exact observer and reconcile locally'})
            page.locator('#ci-read').click()
            expect(page.locator('#ci-tracks')).to_contain_text('reconciliation')
            assert page.locator('[data-ci-action]').count() == 0
            page.screenshot(path=str(SCREENSHOTS / f'ci-unknown-{width}.png'), full_page=True)
            page.locator('[data-task-id="2"]').click()
            expect(page.locator('#detail-ci')).not_to_be_visible()
            page.locator('#logout').click()
            expect(page.locator('#ci-scope')).to_have_text('')
            assert len(posts) == 4
            assert not errors, errors
            context.close()
        browser.close()
    print(f'PASS: desktop/mobile exact-HEAD CI policy/start/replay, stop/resume, evidence and unknown gates; screenshots: {SCREENSHOTS}')
finally:
    server.shutdown()
