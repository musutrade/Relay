"""Optional Chromium fixture: python3 app/tests/ui_publication_smoke.py.
All API traffic is intercepted; no model, repository execution or GitHub mutation.
"""
import json
import os
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

HTML = (Path(__file__).resolve().parents[1] / 'static' / 'index.html').read_bytes()
SCREENSHOTS = Path(os.environ.get('RELAY_UI_SCREENSHOTS') or tempfile.mkdtemp(prefix='relay-publication-'))
SCREENSHOTS.mkdir(parents=True, exist_ok=True)


class Server(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.end_headers()
        self.wfile.write(HTML)

    def log_message(self, *_args):
        pass


def task(task_id, key=None, job=None):
    return {'id': task_id, 'key': key or f'task-{task_id}', 'payload': json.dumps(job or {
        'repository': 'fixture', 'requirements': 'Publish the audited candidate', 'agent': 'fake',
        'test': 'test', 'publish': False}), 'state': 'finished' if task_id == 1 else 'queued',
        'generation': 1 if task_id == 1 else 0, 'owner': 'fixture' if task_id == 1 else None,
        'result': json.dumps({'outcome': 'success', 'workspace': '/fixture', 'draft_pr': None}) if task_id == 1 else None,
        'continuation_status': None}


def publication_action():
    return {'id': 'publish_approved', 'allowed': True, 'ordinary_allowed': True,
        'candidate_sha': 'a' * 40, 'base_sha': 'b' * 40, 'github_repository': 'fixture/project',
        'base_branch': 'main', 'draft_pr_adapter': 'fake-publisher', 'publisher_binding': 'c' * 64,
        'draft': True, 'dry_run': False, 'requires_prior_test_acceptance': True,
        'authorization_ttl_seconds': 86400, 'expires_at_unix_seconds': None}


def operator(value, successor=None):
    return {'task_id': value['id'], 'generation': value['generation'], 'failure': None,
        'resources': {'usage': {'logical_bytes': 500, 'complete': True, 'measured_at': 1700000000, 'reason': None},
            'quota_bytes': 1000, 'host_policy_cap_bytes': 1000, 'snapshot_cap_bytes': 1000},
        'retained_result': {'available': value['result'] is not None, 'immutable': True},
        'workspace_retained': True, 'recovery': {'inherited_quota_bytes': 1000,
            'actions': [publication_action()] if value['id'] == 1 and successor is None else [],
            'blocked_reason': None, 'successor_id': successor, 'reserved_request': None}}


server = ThreadingHTTPServer(('127.0.0.1', 0), Server)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'), headless=True, args=['--no-sandbox'])
        for width, height in [(1440, 1150), (390, 844)]:
            context = browser.new_context(viewport={'width': width, 'height': height}, locale='zh-CN')
            page = context.new_page()
            errors, requests = [], []
            data = {'tasks': [task(2), task(1)], 'mode': 'lost', 'successor': None}
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
                        response = data['tasks']
                    elif path.endswith('/operator'):
                        value = next(value for value in data['tasks'] if value['id'] == int(path.split('/')[-2]))
                        response = operator(value, data['successor'] if value['id'] == 1 else None)
                    elif path.startswith('/api/tasks/') and path.endswith(('/merge-preview', '/merge-authorizations')):
                        assert req.method == 'GET' and not req.post_data
                        task_id = int(path.split('/')[-2])
                        assert any(value['id'] == task_id for value in data['tasks'])
                        response = [] if path.endswith('/merge-authorizations') else {
                            'eligible': False, 'reason': 'Merge authorization disabled in fixture',
                            'policies': [], 'authorizations': [], 'risk_disclosure': None,
                            'lane_diagnostic': None}
                    elif path.startswith('/api/merge-authorizations/'):
                        assert req.method == 'GET' and not req.post_data
                        intercept.fulfill(status=404, content_type='application/json',
                            body=json.dumps({'error': 'Merge authorization does not exist'}))
                        return
                    elif path.startswith('/api/tasks/') and path.endswith(('/ci-preview', '/ci-tracks')):
                        assert req.method == 'GET' and not req.post_data
                        task_id = int(path.split('/')[-2])
                        assert any(value['id'] == task_id for value in data['tasks'])
                        # This fixture stops at publication admission, without a real PR receipt.
                        response = [] if path.endswith('/ci-tracks') else {
                            'eligible': False, 'reason': 'Fixture has no real publication receipt',
                            'publication': None, 'policies': [], 'tracks': [],
                            'remote_merge_eligibility': 'not_established'}
                    elif path.startswith('/api/ci-tracks/'):
                        assert req.method == 'GET' and not req.post_data
                        intercept.fulfill(status=404, content_type='application/json',
                            body=json.dumps({'error': 'CI track does not exist'}))
                        return
                    elif path == '/api/tasks/1/publish-approved':
                        assert req.method == 'POST'
                        body = req.post_data_json
                        requests.append(body)
                        expected = {key: publication_action()[key] for key in [
                            'candidate_sha', 'github_repository', 'base_branch', 'draft_pr_adapter', 'publisher_binding']}
                        assert body == dict(expected, key=body['key'], confirm_publish=True, accept_prior_test_evidence=True)
                        assert body['key']
                        if data['mode'] == 'lost':
                            intercept.abort('failed')
                            return
                        job = json.loads(data['tasks'][-1]['payload'])
                        job['continuation'] = {'predecessor_task_id': 1, 'publish_approved': {'request': body}}
                        response = task(3, body['key'], job)
                        data['tasks'].insert(0, response)
                        data['tasks'][-1]['continuation_status'] = {'successor_id': 3}
                        data['successor'] = 3
                    else:
                        response = next(value for value in data['tasks'] if value['id'] == int(path.split('/')[-1]))
                intercept.fulfill(status=200, content_type='application/json', body=json.dumps(response))

            page.route('**/api/**', route)
            page.route('**/auth/**', route)
            page.goto(base)
            page.locator('#token').fill('fixture-token')
            page.locator('#auth-form button[type=submit]').click()
            page.locator('[data-task-id="1"]').click()
            expect(page.locator('#publish-task')).to_be_enabled()
            page.locator('#publish-task').click()
            expect(page.locator('#publish-dialog')).to_be_visible()
            expect(page.locator('#publish-confirm')).to_be_disabled()
            expect(page.locator('#publish-scope')).to_contain_text('a' * 40)
            expect(page.locator('#publish-scope')).to_contain_text('fixture/project')
            expect(page.locator('#publish-scope')).to_contain_text('不会合并')
            expect(page.locator('#publish-dialog')).to_contain_text('外部输入未冻结，也不会重新验证')
            assert page.locator('#publish-dialog input[type=text]').count() == 0
            page.locator('#publish-accept-tests').check()
            expect(page.locator('#publish-confirm')).to_be_enabled()
            assert page.locator('#publish-dialog').evaluate('(e) => e.scrollWidth <= e.clientWidth')
            page.screenshot(path=str(SCREENSHOTS / f'publication-dialog-{width}.png'), full_page=True)
            page.keyboard.press('Escape')
            expect(page.locator('#publish-dialog')).not_to_be_visible()
            assert not requests
            page.locator('#publish-task').click()
            expect(page.locator('#publish-accept-tests')).not_to_be_checked()
            page.locator('#publish-dismiss').click()
            assert not requests
            page.locator('#publish-task').click()
            page.evaluate("window.dispatchEvent(new PopStateEvent('popstate'))")
            expect(page.locator('#publish-dialog')).not_to_be_visible()
            assert not requests
            page.locator('#publish-task').click()
            page.locator('#publish-accept-tests').check()
            page.locator('#publish-confirm').click()
            expect(page.locator('#detail-error')).to_contain_text('尚未确认')
            assert len(requests) == 1
            first = requests[0].copy()
            page.locator('#publish-task').click()
            expect(page.locator('#publish-accept-tests')).not_to_be_checked()
            data['mode'] = 'success'
            page.locator('#publish-accept-tests').check()
            page.locator('#publish-confirm').click()
            expect(page.locator('#detail-title')).to_have_text('任务 #3')
            assert requests == [first, first]
            expect(page.locator('#continuation-choice')).to_contain_text('不重跑开发、审查或测试')
            expect(page.locator('#detail-meta')).to_contain_text('仅发布已批准候选')
            page.locator('[data-task-id="1"]').click()
            expect(page.locator('#publish-task')).not_to_be_visible()
            expect(page.locator('#continuation-next')).to_be_visible()
            assert not errors, errors
            context.close()
        browser.close()
    print(f'PASS: desktop/mobile publication confirmation, cancel/history dismissal and exact retry request; screenshots: {SCREENSHOTS}')
finally:
    server.shutdown()
