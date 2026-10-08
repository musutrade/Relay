"""Desktop/mobile merge-authorization fixture; all APIs are intercepted.

No model, GitHub, real merge, repository or permission changes are made. Run this
in the existing Playwright CI job, never on a socket-restricted local host.
"""
import copy
import hashlib
import json
import os
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

HTML = (Path(__file__).resolve().parents[1] / 'static' / 'index.html').read_bytes()
SCREENSHOTS = Path(os.environ.get('RELAY_UI_SCREENSHOTS') or tempfile.mkdtemp(prefix='relay-merge-'))
SCREENSHOTS.mkdir(parents=True, exist_ok=True)
HEAD = 'a' * 40
POLICY_DIGEST = 'b' * 64
SCOPE_DIGEST = 'c' * 64
CI_DIGEST = 'd' * 64
HOSTILE_TEXT = '<img src=x onerror="window.fixtureInjected = true">'
DISCLOSURE_TEXT = (
    'The repository and PR numeric identities, exact HEAD, target branch, squash method, '
    'CI source and deadline are fixed by this authorization. GitHub checks the target '
    'branch only at preflight; the target may change before the merge and this guard '
    'is not atomic. The deadline is the last time a ready or merge request may be '
    'dispatched, not a guarantee that an accepted request completes before expiry. '
    'Revocation cannot cancel an already accepted asynchronous merge request. Existing '
    'GitHub automation may still act. Acceptance or pending status is not a merged result. '
    'The merge request does not bypass repository rules. Ready-for-review is optional '
    'and requires its own explicit opt-in.'
)
DISCLOSURE = {'version': 1, 'sha256': hashlib.sha256(DISCLOSURE_TEXT.encode()).hexdigest(),
    'text': DISCLOSURE_TEXT}
TARGET = {'repository': 'fixture/project', 'repository_id': 801, 'pr_number': 19,
    'pr_id': 901, 'pr_node_id': None, 'pr_url': 'https://github.com/fixture/project/pull/19',
    'head_sha': HEAD, 'head_branch': 'relay/task-1-g1', 'base_branch': 'main'}


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
        'repository': 'fixture', 'requirements': 'Explicitly authorize an exact-HEAD merge',
        'agent': 'fake', 'test': 'test', 'publish': False}),
        'state': 'finished' if task_id == 1 else 'queued',
        'generation': 1 if task_id == 1 else 0, 'owner': 'fixture' if task_id == 1 else None,
        'result': json.dumps({'outcome': 'success', 'workspace': '/fixture'}) if task_id == 1 else None,
        'continuation_status': None}


def operator(value):
    return {'task_id': value['id'], 'generation': value['generation'], 'failure': None,
        'resources': {'usage': {'logical_bytes': 500, 'complete': True,
            'measured_at': 1700000000, 'reason': None}, 'quota_bytes': 1000,
            'host_policy_cap_bytes': 1000, 'snapshot_cap_bytes': 1000},
        'retained_result': {'available': value['result'] is not None, 'immutable': True},
        'workspace_retained': True, 'recovery': {'inherited_quota_bytes': 1000,
            'actions': [], 'blocked_reason': None, 'successor_id': None, 'reserved_request': None}}


def disabled_preview():
    return {'eligible': False, 'reason': 'Merge authorization disabled in fixture',
        'policies': [], 'authorizations': [], 'risk_disclosure': None, 'lane_diagnostic': None}


def preview():
    return {'eligible': True, 'reason': None, 'policies': [{
        'name': 'exact-head-squash', 'policy_digest': POLICY_DIGEST, 'scope_digest': SCOPE_DIGEST,
        'ci_track_id': 10, 'ci_policy': 'required-ci', 'ci_policy_digest': CI_DIGEST,
        'ci_source': {'workflow_id': 41, 'app_id': 15368, 'event': 'pull_request',
            'required_jobs': ['check', 'browser']},
        'merge_method': 'squash', 'allow_ready': True, 'authorization_window_seconds': 3600,
        'poll_interval_seconds': 60, 'deadline': int(time.time()) + 3600,
        'target_guard': 'preflight_only', 'target': copy.deepcopy(TARGET)}],
        'risk_disclosure': copy.deepcopy(DISCLOSURE), 'authorizations': [], 'lane_diagnostic': None}


def ci_preview(identity_ready=True):
    now = int(time.time())
    publication = {key: TARGET[key] for key in ('repository', 'base_branch', 'head_sha',
        'head_branch', 'pr_number', 'pr_url')}
    policy = {'name': 'required-ci', 'policy_digest': CI_DIGEST, 'workflow_id': 41,
        'app_id': 15368, 'event': 'pull_request', 'required_jobs': ['check', 'browser'],
        'poll_interval_seconds': 60, 'observation_window_seconds': 3600}
    track = dict(publication, **policy, id=10, publication_task_id=1, policy='required-ci',
        status='watching', attempt=1 if identity_ready else 0, revision=2 if identity_ready else 1,
        stop_requested=False, created_at=now, deadline=now + 3600,
        next_poll_at=now + 60, last_observed_at=now if identity_ready else None,
        window_generation=1, window_started_at=now, diagnostic=None,
        observed_repository_id=801 if identity_ready else None,
        observed_pr_id=901 if identity_ready else None,
        observed_base_sha='b' * 40 if identity_ready else None,
        latest_evidence={'version': 1, 'observation': 'ok', 'complete': True,
            'remote_merge_eligibility': 'not_established', 'observed_at': now,
            'missing_jobs': ['check', 'browser'], 'failed_jobs': [], 'run': None}
            if identity_ready else None,
        remote_merge_eligibility='not_established')
    return {'eligible': True, 'reason': None, 'publication': publication,
        'policies': [policy], 'tracks': [track], 'remote_merge_eligibility': 'not_established'}


def expected_request(policy, key, allow_ready=False):
    return {'key': key, 'confirm_merge': True, 'policy': policy['name'],
        'policy_digest': policy['policy_digest'], 'ci_track_id': policy['ci_track_id'],
        'scope_digest': policy['scope_digest'], 'deadline': policy['deadline'],
        'allow_ready': allow_ready, 'accept_non_atomic_target_guard': True,
        'deadline_semantics': 'last_dispatch', 'accept_existing_automation': True,
        'risk_disclosure_version': DISCLOSURE['version'],
        'risk_disclosure_sha256': DISCLOSURE['sha256']}


def authorization(policy, body):
    created_at = policy['deadline'] - policy['authorization_window_seconds']
    return {'id': 20, 'publication_task_id': 1, 'ci_track_id': policy['ci_track_id'],
        'policy': policy['name'], 'policy_digest': policy['policy_digest'],
        'scope_digest': policy['scope_digest'],
        'authorization_window_seconds': policy['authorization_window_seconds'],
        'status': 'watching', 'revision': 1, 'attempt': 0, 'revoked': False,
        'target': copy.deepcopy(policy['target']), 'ci_policy': policy['ci_policy'],
        'ci_policy_digest': policy['ci_policy_digest'], 'ci_source': copy.deepcopy(policy['ci_source']),
        'poll_interval_seconds': policy['poll_interval_seconds'], 'merge_method': 'squash',
        'target_guard': 'preflight_only', 'created_at': created_at,
        'deadline': body['deadline'], 'allow_ready': body['allow_ready'],
        'consent': {key: value for key, value in body.items() if key != 'key'},
        'async_request': None, 'merge_commit_sha': None, 'latest_evidence': None, 'diagnostic': None,
        'next_poll_at': created_at, 'last_observed_at': None,
        'ready_dispatched': False, 'merge_dispatched': False,
        'ready_confirmed': False, 'merge_failed_confirmed': False,
        'remote_merge_eligibility': 'not_established', 'resolved_pr_node_id': None,
        'observation_only': False, 'observed_remote_status': None}


class Fixture:
    def __init__(self, browser, base, width, height, identity_ready=True):
        self.context = browser.new_context(viewport={'width': width, 'height': height}, locale='zh-CN')
        self.page = self.context.new_page()
        self.base = base
        self.width = width
        self.errors, self.posts, self.reads = [], [], []
        self.merge = preview()
        self.original_policy = copy.deepcopy(self.merge['policies'][0])
        self.ci = ci_preview(identity_ready)
        if not identity_ready:
            self.merge = disabled_preview()
            self.merge['reason'] = 'Waiting for the first trustworthy numeric CI identity observation'
        self.mode = 'success'
        self.reconcile_status = None
        self.page.on('pageerror', lambda error: self.errors.append(str(error)))
        self.page.route('**/api/**', self.route)
        self.page.route('**/auth/**', self.route)
        # A fixture must never reach GitHub, a model or any other external site.
        self.context.route('https://**/*', self.reject_external)
        self.page.goto(base)
        self.connect(identity_ready)

    @staticmethod
    def reject_external(intercept):
        intercept.abort('blockedbyclient')
        raise AssertionError(f'Unexpected external browser request: {intercept.request.url}')

    def route(self, intercept):
        req = intercept.request
        assert req.url.startswith(self.base + '/')
        path = req.url.removeprefix(self.base)
        if path == '/auth/status':
            response = {'mode': 'bearer', 'authenticated': False}
        else:
            assert req.headers.get('authorization') == 'Bearer fixture-token'
            if req.method == 'GET':
                assert not req.post_data
                self.reads.append(path)
                if path == '/api/config':
                    response = {'repositories': ['fixture'], 'agents': ['fake'], 'tests': ['test']}
                elif path == '/api/status':
                    response = {'active': None, 'recovery_required': False, 'diagnostic': None}
                elif path == '/api/tasks':
                    response = [task(2), task(1)]
                elif path in ('/api/tasks/1/operator', '/api/tasks/2/operator'):
                    response = operator(task(int(path.split('/')[-2])))
                elif path in ('/api/tasks/1/merge-preview', '/api/tasks/2/merge-preview'):
                    response = self.merge if path.startswith('/api/tasks/1/') else disabled_preview()
                elif path in ('/api/tasks/1/merge-authorizations', '/api/tasks/2/merge-authorizations'):
                    response = self.merge['authorizations'] if path.startswith('/api/tasks/1/') else []
                elif path.startswith('/api/merge-authorizations/'):
                    record_id = int(path.split('/')[-1])
                    response = next((record for record in self.merge['authorizations']
                        if record['id'] == record_id), None)
                    if response is None:
                        intercept.fulfill(status=404, content_type='application/json',
                            body=json.dumps({'error': 'Merge authorization does not exist'}))
                        return
                elif path in ('/api/tasks/1/ci-preview', '/api/tasks/2/ci-preview'):
                    response = self.ci if path.startswith('/api/tasks/1/') else {
                        'eligible': False, 'reason': 'No CI for this queued task',
                        'publication': None, 'policies': [], 'tracks': [],
                        'remote_merge_eligibility': 'not_established'}
                elif path in ('/api/tasks/1/ci-tracks', '/api/tasks/2/ci-tracks'):
                    response = []
                elif path.startswith('/api/ci-tracks/'):
                    intercept.fulfill(status=404, content_type='application/json',
                        body=json.dumps({'error': 'CI track does not exist'}))
                    return
                elif path in ('/api/tasks/1', '/api/tasks/2'):
                    response = task(int(path.split('/')[-1]))
                else:
                    raise AssertionError(f'Unexpected read: {path}')
            else:
                assert req.method == 'POST'
                body = req.post_data_json
                self.posts.append((path, copy.deepcopy(body)))
                if path == '/api/tasks/1/authorize-merge':
                    assert isinstance(body.get('key'), str) and body['key']
                    assert body == expected_request(self.original_policy, body['key'], body['allow_ready'])
                    assert isinstance(body['allow_ready'], bool)
                    if self.mode == 'lost':
                        intercept.abort('failed')
                        return
                    if not self.merge['authorizations']:
                        self.merge['authorizations'] = [authorization(self.original_policy, body)]
                    response = self.merge['authorizations'][0]
                elif path in ('/api/merge-authorizations/20/revoke', '/api/merge-authorizations/20/reconcile'):
                    current = self.merge['authorizations'][0]
                    assert body == {'expected_revision': current['revision']}
                    current['revision'] += 1
                    if path.endswith('/revoke'):
                        current.update(status='revoked', revoked=True, next_poll_at=None)
                    elif self.reconcile_status is not None:
                        current['status'] = self.reconcile_status
                    response = current
                else:
                    raise AssertionError(f'Unexpected mutation: {path}')
        intercept.fulfill(status=200, content_type='application/json', body=json.dumps(response))

    def connect(self, expect_enabled=True):
        self.page.locator('#token').fill('fixture-token')
        self.page.locator('#auth-form button[type=submit]').click()
        expect(self.page.locator('#auth-panel')).to_be_hidden()
        self.page.locator('[data-task-id="1"]').click()
        if expect_enabled:
            expect(self.page.locator('#merge-authorize')).to_be_enabled()
        else:
            expect(self.page.locator('#merge-status')).to_contain_text('numeric CI identity')
            expect(self.page.locator('#merge-authorize')).not_to_be_enabled()

    def open_dialog(self):
        self.page.locator('#merge-authorize').click()
        expect(self.page.locator('#merge-dialog')).to_be_visible()
        expect(self.page.locator('dialog[open]')).to_have_count(1)
        expect(self.page.locator('#merge-accept')).not_to_be_checked()
        expect(self.page.locator('#merge-confirm')).to_be_disabled()

    def refresh(self):
        with self.page.expect_response(lambda response: response.url == self.base + '/api/tasks/1/merge-preview'):
            self.page.locator('#merge-read').click()
        expect(self.page.locator('#merge-read')).to_be_enabled()

    def close(self):
        assert not self.errors, self.errors
        assert self.page.evaluate('window.fixtureInjected === undefined')
        self.context.close()


def confirmation_and_replay(browser, base, width, height):
    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    fixture.open_dialog()
    dialog_scope = page.locator('#merge-dialog-scope')
    for value in ('fixture/project', '801', '901', '19', HEAD, 'relay/task-1-g1', 'main',
                  POLICY_DIGEST, SCOPE_DIGEST, CI_DIGEST, 'required-ci', '41', '15368',
                  'pull_request', 'check', 'browser', 'squash', '3600'):
        expect(dialog_scope).to_contain_text(value)
    expect(page.locator('#merge-disclosure')).to_have_text(DISCLOSURE_TEXT)
    expect(page.locator('#merge-allow-ready')).not_to_be_checked()
    assert page.locator('#merge-dialog input[type=text]').count() == 0
    assert not fixture.posts
    page.locator('#merge-allow-ready').check()
    expect(page.locator('#merge-confirm')).to_be_disabled()
    page.locator('#merge-accept').check()
    expect(page.locator('#merge-confirm')).to_be_enabled()
    assert page.locator('#merge-dialog').evaluate('(e) => e.scrollWidth <= e.clientWidth')
    page.screenshot(path=str(SCREENSHOTS / f'merge-confirmation-{width}.png'), full_page=True)
    page.keyboard.press('Escape')
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    fixture.open_dialog()
    expect(page.locator('#merge-allow-ready')).not_to_be_checked()
    page.locator('#merge-dismiss').click()
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    fixture.open_dialog()
    page.locator('#merge-accept').check()
    page.evaluate("window.dispatchEvent(new PopStateEvent('popstate'))")
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    assert not fixture.posts
    fixture.refresh()
    fixture.open_dialog()
    page.locator('#merge-accept').check()
    page.locator('[data-task-id="2"]').evaluate('(button) => button.click()')
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    expect(page.locator('#detail-merge')).not_to_be_visible()
    page.locator('[data-task-id="1"]').click()
    expect(page.locator('#merge-authorize')).to_be_enabled()
    fixture.open_dialog()
    page.locator('#merge-accept').check()
    page.locator('#logout').evaluate('(button) => button.click()')
    expect(page.locator('#auth-panel')).to_be_visible()
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    assert not fixture.posts
    fixture.connect()
    fixture.open_dialog()
    expect(page.locator('#merge-allow-ready')).not_to_be_checked()
    fixture.mode = 'lost'
    page.locator('#merge-accept').check()
    # Both events fire in one JavaScript turn; only one request may be dispatched.
    page.locator('#merge-confirm').evaluate('(button) => { button.click(); button.click(); }')
    expect(page.locator('#merge-error')).to_contain_text('尚未确认')
    assert len(fixture.posts) == 1
    first = copy.deepcopy(fixture.posts[0])
    assert first[1]['allow_ready'] is False
    assert first[1]['deadline_semantics'] == 'last_dispatch'
    fixture.merge['policies'][0]['deadline'] += 60
    fixture.refresh()
    assert len(fixture.posts) == 1, 'Reading local records must not retry authorization'
    fixture.mode = 'success'
    # The already-authorized frozen request retries directly, without creating
    # another consent or silently changing its original dispatch deadline.
    page.locator('#merge-authorize').click()
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    expect(page.locator('#merge-records')).to_contain_text('20')
    assert fixture.posts == [first, first], 'Lost-response replay changed the frozen request'
    assert page.locator('#detail-merge').evaluate('(e) => e.scrollWidth <= e.clientWidth')
    fixture.close()


def first_identity_while_pending(browser, base, width, height):
    fixture = Fixture(browser, base, width, height, identity_ready=False)
    page = fixture.page
    expect(page.locator('#ci-tracks')).to_contain_text('Repository ID：尚未建立')
    expect(page.locator('#merge-authorize')).not_to_be_enabled()
    assert not fixture.posts
    # A first complete identity observation can arrive before any configured job
    # is done. This enables consent, but is not cached-green merge eligibility.
    current = fixture.ci['tracks'][0]
    now = int(time.time())
    current.update(revision=2, attempt=1, observed_repository_id=801, observed_pr_id=901,
        observed_base_sha='b' * 40, last_observed_at=now,
        latest_evidence={'version': 1, 'observation': 'ok', 'complete': True,
            'remote_merge_eligibility': 'not_established', 'observed_at': now,
            'missing_jobs': ['check', 'browser'], 'failed_jobs': [], 'run': None})
    fixture.merge = preview()
    fixture.original_policy = copy.deepcopy(fixture.merge['policies'][0])
    page.locator('#ci-read').click()
    expect(page.locator('#ci-tracks')).to_contain_text('Repository ID：801')
    expect(page.locator('#ci-tracks')).to_contain_text('缺失检查：check、browser')
    fixture.refresh()
    fixture.open_dialog()
    page.locator('#merge-allow-ready').check()
    page.locator('#merge-accept').check()
    page.locator('#merge-confirm').click()
    expect(page.locator('#merge-records')).to_contain_text('已授权，等待最新精确 HEAD 检查')
    assert fixture.posts[0][1]['allow_ready'] is True
    authorized = fixture.merge['authorizations'][0]
    authorized.update(ready_dispatched=True, ready_confirmed=True, revision=2)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('转正已确认：是')
    expect(page.locator('#ci-tracks')).to_contain_text('缺失检查：check、browser')
    expect(page.locator('#merge-records')).not_to_contain_text('已确认 Relay 请求合并完成')
    assert authorized['async_request'] is None and len(fixture.posts) == 1
    page.screenshot(path=str(SCREENSHOTS / f'merge-identity-before-checks-{width}.png'), full_page=True)
    fixture.close()


def ready_opt_in(browser, base, width, height):
    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    fixture.open_dialog()
    page.locator('#merge-allow-ready').check()
    page.locator('#merge-accept').check()
    page.locator('#merge-confirm').click()
    expect(page.locator('#merge-records')).to_contain_text('已授权，等待最新精确 HEAD 检查')
    assert len(fixture.posts) == 1 and fixture.posts[0][1]['allow_ready'] is True
    expect(page.locator('#merge-authorize')).not_to_be_visible()
    fixture.close()


def accepted_record(fixture, status='accepted'):
    policy = fixture.original_policy
    record = authorization(policy, expected_request(policy, 'fixture-accepted-key'))
    record.update(status=status, attempt=1, merge_dispatched=True,
        async_request={'id': '123e4567-e89b-42d3-a456-426614174000',
            'options': {'sha': HEAD, 'merge_method': 'squash',
                'merge_action': 'direct_merge', 'bypass_rules': False}, 'provenance': 'relay'})
    fixture.merge['authorizations'] = [record]
    return record


def async_states_and_reconcile(browser, base, width, height):
    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    current = accepted_record(fixture)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('请求已受理，等待 GitHub（尚未合并）')
    expect(page.locator('#merge-records')).not_to_contain_text('已确认 Relay 请求合并完成')
    expect(page.locator('#merge-records')).to_contain_text(current['async_request']['id'])
    assert not fixture.posts
    current.update(status='pending', revision=current['revision'] + 1)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('GitHub 请求处理中（尚未合并）')
    expect(page.locator('#merge-records')).not_to_contain_text('已确认 Relay 请求合并完成')
    assert not fixture.posts
    page.screenshot(path=str(SCREENSHOTS / f'merge-pending-{width}.png'), full_page=True)
    page.locator('[data-merge-action="revoke"]').click()
    expect(page.locator('#merge-records')).to_contain_text('撤销')
    assert current['revoked'] is True
    assert len(fixture.posts) == 1
    assert fixture.posts[0][0] == '/api/merge-authorizations/20/revoke'
    # Revocation blocks future dispatch. It cannot cancel an async request
    # GitHub already accepted; a later read may truthfully report its merge.
    current.update(status='merged', revision=current['revision'] + 1,
        last_observed_at=int(time.time()))
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('已确认 Relay 请求合并完成')
    assert current['revoked'] is True
    assert len(fixture.posts) == 1, 'Late merge observation must not dispatch a new mutation'
    assert page.locator('[data-merge-action="revoke"]').count() == 0
    page.screenshot(path=str(SCREENSHOTS / f'merge-late-after-revoke-{width}.png'), full_page=True)
    fixture.close()

    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    current = accepted_record(fixture, 'effect_unknown')
    current['diagnostic'] = {'code': 'merge_response_unknown',
        'message': 'Read-only reconciliation required: ' + HOSTILE_TEXT}
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('远端影响未知，仅可只读核对')
    expect(page.locator('#merge-records')).to_contain_text(HOSTILE_TEXT)
    assert page.locator('#merge-records img').count() == 0
    assert not fixture.posts
    fixture.refresh()
    assert not fixture.posts, 'Reading unknown state must not reconcile automatically'
    fixture.reconcile_status = 'pending'
    page.locator('[data-merge-action="reconcile"]').evaluate(
        '(button) => { button.click(); button.click(); }')
    expect(page.locator('#merge-records')).to_contain_text('GitHub 请求处理中（尚未合并）')
    assert len(fixture.posts) == 1
    assert fixture.posts[0][0] == '/api/merge-authorizations/20/reconcile'
    assert fixture.posts[0][1] == {'expected_revision': 1}
    fixture.close()


def expiry_and_identity(browser, base, width, height):
    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    fixture.open_dialog()
    page.locator('#merge-accept').check()
    expect(page.locator('#merge-confirm')).to_be_enabled()
    page.clock.set_fixed_time((fixture.original_policy['deadline'] + 1) * 1000)
    # Even a queued/programmatic click must recheck the deadline in the handler.
    page.locator('#merge-confirm').dispatch_event('click')
    expect(page.locator('#merge-dialog')).not_to_be_visible()
    assert not fixture.posts, 'Expired dialog consent must never authorize a merge'
    fixture.close()

    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    current = accepted_record(fixture, 'expired')
    page.clock.set_fixed_time((current['deadline'] + 1) * 1000)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('期限')
    expect(page.locator('#merge-records')).not_to_contain_text('已确认 Relay 请求合并完成')
    assert not fixture.posts
    current.update(status='merged', revision=current['revision'] + 1,
        last_observed_at=current['deadline'] + 2)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('已确认 Relay 请求合并完成')
    assert not fixture.posts, 'Late completion after deadline is a read, not new authorization'
    page.screenshot(path=str(SCREENSHOTS / f'merge-late-after-expiry-{width}.png'), full_page=True)
    fixture.close()

    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    current = accepted_record(fixture)
    fixture.refresh()
    expect(page.locator('#merge-records')).to_contain_text('请求已受理，等待 GitHub（尚未合并）')
    # Same repository spelling and PR number do not excuse changed numeric identity.
    current['target']['repository_id'] += 1
    current['revision'] += 1
    fixture.refresh()
    expect(page.locator('#merge-error')).to_be_visible()
    expect(page.locator('#merge-authorize')).not_to_be_enabled()
    assert page.locator('[data-merge-action]').count() == 0
    assert not fixture.posts
    fixture.close()

    fixture = Fixture(browser, base, width, height)
    page = fixture.page
    fixture.merge['risk_disclosure']['sha256'] = '0' * 64
    fixture.refresh()
    expect(page.locator('#merge-error')).to_be_visible()
    expect(page.locator('#merge-authorize')).not_to_be_enabled()
    assert not fixture.posts, 'Unverified risk disclosure must never authorize a merge'
    fixture.close()


server = ThreadingHTTPServer(('127.0.0.1', 0), Server)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f'http://127.0.0.1:{server.server_port}'
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'),
            headless=True, args=['--no-sandbox'])
        for width, height in [(1440, 1150), (390, 844)]:
            confirmation_and_replay(browser, base, width, height)
            first_identity_while_pending(browser, base, width, height)
            ready_opt_in(browser, base, width, height)
            async_states_and_reconcile(browser, base, width, height)
            expiry_and_identity(browser, base, width, height)
        browser.close()
    print(f'PASS: desktop/mobile merge consent, frozen replay, async states, revoke/expiry, read-only reconcile and identity gates; screenshots: {SCREENSHOTS}')
finally:
    server.shutdown()
