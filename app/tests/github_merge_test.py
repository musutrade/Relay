"""Offline async merge contracts and fail-closed effects; no live GitHub calls."""
import contextlib
import copy
import fcntl
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("github_merge", ROOT / "examples/github-merge.py")
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)
FIXTURES = importlib.util.spec_from_file_location(
    "ci_observer_fixtures", ROOT / "app/tests/github_ci_observer_test.py")
fixtures = importlib.util.module_from_spec(FIXTURES)
FIXTURES.loader.exec_module(fixtures)
HEAD, BASE, OTHER, REPO = fixtures.HEAD, fixtures.BASE, fixtures.OTHER, fixtures.REPO
PREFIX, PR = fixtures.PREFIX, fixtures.PR_ENDPOINT
MERGE = f"{PR}/merge-async"
UUID = "630b9d5e-3f2a-4f7e-8b0c-2d5f9a8c1e42"
NOW = 1_791_425_000


def request(operation="preflight"):
    return {"version": 1, "operation": operation, "authorization_id": 41, "attempt": 1,
            "repository": REPO, "repository_id": 11, "pr_number": 7, "pr_id": 13,
            "pr_node_id": None if operation == "preflight" else "PR_fixed_13",
            "pr_url": f"https://github.com/{REPO}/pull/7", "head_sha": HEAD,
            "head_branch": "relay/task-1-g1", "base_branch": "main",
            "ci_source": {"workflow_id": 19, "app_id": 15368, "event": "pull_request",
                          "required_jobs": ["test"]}, "merge_method": "squash",
            "target_guard": "preflight_only", "allow_ready": False,
            "accept_non_atomic_target_guard": True, "deadline_semantics": "last_dispatch",
            "deadline": NOW + 3600, "async_request": None}


def accepted(request_value=None, provenance="relay"):
    return {"id": UUID, "options": adapter.options(request_value or request()), "provenance": provenance}


def pending():
    return {"status": "pending", "details": {"uuid": UUID, "expected_head_sha": HEAD,
            "merge_method": "squash", "merge_action": "direct_merge", "bypass_rules": False,
            "message": "Merge request is in progress."}}


def response(data, code=200, headers=None):
    _, raw = fixtures.response(data, code, headers)
    return (0 if code < 400 else 1), raw


def fields(command):
    result = {}
    for index, value in enumerate(command[:-1]):
        if value in ("--raw-field", "--field"):
            key, text = command[index + 1].split("=", 1)
            result[key] = text if value == "--raw-field" else json.loads(text)
    return result


class Fixture(fixtures.Fixture):
    def __init__(self):
        super().__init__()
        self.pr.update(node_id="PR_fixed_13", draft=False)
        self.repo = {"id": 11, "full_name": REPO, "delete_branch_on_merge": True,
                     "allow_merge_commit": True, "allow_squash_merge": True, "allow_rebase_merge": True}
        gql_repo = {"databaseId": 11, "nameWithOwner": REPO}
        self.gql_pr = {"id": "PR_fixed_13", "fullDatabaseId": "13", "number": 7,
                       "url": f"https://github.com/{REPO}/pull/7", "state": "OPEN",
                       "merged": False, "isDraft": False, "headRefName": "relay/task-1-g1",
                       "headRefOid": HEAD, "baseRefName": "main", "baseRefOid": BASE,
                       "headRepository": copy.deepcopy(gql_repo), "baseRepository": copy.deepcopy(gql_repo),
                       "baseRef": {"name": "main", "target": {"oid": BASE}},
                       "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
                       "stack": None, "stackEntry": None, "isInMergeQueue": False,
                       "isMergeQueueEnabled": False, "mergeQueue": None, "mergeQueueEntry": None,
                       "autoMergeRequest": None}
        self.gql = {"data": {"repository": {**gql_repo, "pullRequest": self.gql_pr}}}
        self.merge_response = response(pending(), 202)
        self.poll_response = response(pending())
        self.merge_hook = None

    def draft(self):
        self.pr["draft"] = self.gql_pr["isDraft"] = True
        self.gql_pr["mergeStateStatus"] = "DRAFT"

    def ci_pending(self):
        for row in (self.run, self.suite, self.job, self.check):
            row.update(status="in_progress", conclusion=None)
        if not self.gql_pr["isDraft"]:
            self.gql_pr["mergeStateStatus"] = "UNSTABLE"

    def writes(self):
        return [c for c in self.calls if c[5] == "PUT" or fields(c).get("query") == adapter.READY_MUTATION]

    def capture(self, command, env):
        method, endpoint, values = command[5], command[-1], fields(command)
        occurrence = sum(c[5] == method and c[-1] == endpoint and fields(c) == values for c in self.calls) + 1
        if self.merge_hook:
            override = self.merge_hook(method, endpoint, values, occurrence)
            if override is not None:
                self.calls.append(command)
                self.environments.append(env)
                return override
        if endpoint == PREFIX:
            reply = response(self.repo)
        elif endpoint == "graphql" and values["query"] == adapter.PREFLIGHT_QUERY:
            reply = response(self.gql)
        elif endpoint == "graphql" and values["query"] == adapter.READY_MUTATION:
            self.pr["draft"] = self.gql_pr["isDraft"] = False
            self.gql_pr["mergeStateStatus"] = "CLEAN"
            reply = response({"data": {"markPullRequestReadyForReview": {"pullRequest": {
                "id": "PR_fixed_13", "fullDatabaseId": "13", "number": 7, "isDraft": False}}}})
        elif endpoint == MERGE:
            reply = self.merge_response
        elif endpoint == f"{MERGE}/{UUID}":
            reply = self.poll_response
        else:
            return super().capture(command, env)
        self.calls.append(command)
        self.environments.append(env)
        return reply


class MergeAdapterTests(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture()
        self.request = request()
        self.env = {"RELAY_GH_PROGRAM": "/host/bin/gh", "RELAY_MERGE_CONTROL": "1",
                    "RELAY_MERGE_WRITE": "1"}
        self.directory = tempfile.TemporaryDirectory(prefix="relay-merge-gate-test-")
        self.addCleanup(self.directory.cleanup)
        self.gate_path = Path(self.directory.name) / "gate.json"
        self.gate_binding = "d" * 64

    def make_gate(self, value, **changes):
        gate = {"version": 1, "authorization_id": value["authorization_id"], "attempt": value["attempt"],
                "phase": value["operation"], "consent_sha256": self.gate_binding,
                "deadline": value["deadline"], "revoked": False, "write_started": False, **changes}
        if self.gate_path.exists():
            self.gate_path.chmod(0o600)
        self.gate_path.write_text(json.dumps(gate))
        self.gate_path.chmod(0o600)
        meta = self.gate_path.stat()
        return {"RELAY_MERGE_GATE_PATH": str(self.gate_path), "RELAY_MERGE_GATE_BINDING": self.gate_binding,
                "RELAY_MERGE_GATE_DEVICE": str(meta.st_dev), "RELAY_MERGE_GATE_INODE": str(meta.st_ino)}

    def execute(self, value=None, env=None, raw=None, auto_gate=True):
        value = self.request if value is None else value
        environment = dict(self.env if env is None else env)
        if auto_gate and value.get("operation") in ("ready", "merge"):
            environment.update(self.make_gate(value))
        source = json.dumps(value).encode() if raw is None else raw
        output = io.StringIO()
        with patch.object(adapter, "capture", side_effect=self.fixture.capture), \
                patch.object(subprocess, "Popen") as process, \
                patch.object(adapter.time, "time", return_value=NOW), \
                contextlib.redirect_stdout(output):
            self.assertEqual(adapter.main(environment, io.BytesIO(source)), 0)
        process.assert_not_called()
        result = json.loads(output.getvalue())
        self.assertEqual(set(result), {"version", "operation", "status", "complete", "effect", "ci_observation",
                                    "target", "async_request", "merge_commit_sha", "error_code", "detail"})
        self.assertLessEqual(len(output.getvalue().encode()), adapter.MAX_OUTPUT + 1)
        return result

    def test_preflight_resolves_same_numeric_pr_node_and_only_reads(self):
        result = self.execute()
        self.assertEqual(result["status"], "preflight_ready", result)
        self.assertEqual(result["effect"], "none")
        self.assertTrue(result["complete"])
        self.assertEqual(result["target"], {"repository_id": 11, "pr_id": 13, "pr_node_id": "PR_fixed_13",
            "head_sha": HEAD, "head_branch": "relay/task-1-g1", "base_branch": "main", "base_sha": BASE,
            "draft": False, "state": "open", "merged": False, "stack_clear": True,
            "queue_clear": True, "auto_merge_disabled": True, "delete_branch_on_merge": True})
        self.assertEqual(result["ci_observation"]["observation"], "ok")
        self.assertEqual(result["ci_observation"]["remote_merge_eligibility"], "not_established")
        self.assertNotIn("node_id", result["ci_observation"]["pull_request"])
        self.assertEqual(self.fixture.writes(), [])
        self.assertEqual(len(self.fixture.calls), 12)

    def test_each_attempt_uses_current_base_not_historical_ci_base(self):
        self.fixture.pr["base"]["sha"] = OTHER
        self.fixture.gql_pr["baseRefOid"] = OTHER
        self.fixture.gql_pr["baseRef"]["target"]["oid"] = OTHER
        # The workflow's historical PR link base remains BASE, and must not be rewritten.
        self.assertEqual(self.fixture.run["pull_requests"][0]["base"]["sha"], BASE)
        result = self.execute(request("merge"))
        self.assertEqual(result["status"], "accepted", result)
        self.assertEqual(result["target"]["base_sha"], OTHER)
        self.assertEqual(self.fixture.run["pull_requests"][0]["base"]["sha"], BASE)

    def test_pending_202_is_acceptance_not_merge_and_only_pinned_non_bypass_write(self):
        result = self.execute(request("merge"))
        self.assertEqual((result["status"], result["effect"]), ("accepted", "merge_request_recorded"), result)
        self.assertEqual(result["async_request"], accepted())
        self.assertIsNone(result["merge_commit_sha"])
        writes = self.fixture.writes()
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0][5], "PUT")
        self.assertEqual(writes[0][-1], MERGE)
        self.assertEqual(fields(writes[0]), {"sha": HEAD, "merge_method": "squash",
                                           "merge_action": "direct_merge", "bypass_rules": False})
        self.assertEqual(fields(self.fixture.calls[-2])["query"], adapter.PREFLIGHT_QUERY)
        for command in self.fixture.calls:
            self.assertEqual(command[:4], ["/host/bin/gh", "api", "--hostname", "github.com"])
            self.assertIn("X-GitHub-Api-Version: 2026-03-10", command)
            self.assertNotIn("--paginate", command)
            self.assertNotIn("--watch", command)

    def test_default_off_invalid_scope_and_missing_ready_permission_spawn_nothing(self):
        cases = [(request(), {}, "merge_control_disabled"),
                 (request("merge"), {"RELAY_MERGE_CONTROL": "1", "RELAY_GH_PROGRAM": "/host/bin/gh"}, "merge_write_disabled"),
                 (request(), {**self.env, "RELAY_GH_PROGRAM": "gh"}, "configuration_invalid"),
                 (request("ready"), self.env, "ready_not_authorized"),
                 ({**request("merge"), "pr_node_id": None}, self.env, "node_identity_unpinned"),
                 ({**request("merge"), "async_request": accepted()}, self.env, "write_already_recorded"),
                 ({**request(), "deadline": NOW}, self.env, "authorization_expired")]
        for value, env, code in cases:
            with self.subTest(code=code):
                result = self.execute(value, env)
                self.assertEqual(result["error_code"], code, result)
                self.assertEqual(self.fixture.calls, [])

    def test_input_is_strict_and_untrusted_fields_never_become_routes(self):
        changes = [{"repository": "evil/../repo"}, {"repository": "evil/repo?x=y"},
                   {"pr_number": True}, {"pr_url": "https://evil.invalid/pull/7"},
                   {"head_sha": HEAD + "?x=y"}, {"base_branch": "../main"},
                   {"pr_node_id": "@/etc/passwd"}, {"authorization_id": 0}, {"attempt": True},
                   {"merge_method": "--admin"}, {"target_guard": "atomic"},
                   {"accept_non_atomic_target_guard": False}, {"deadline_semantics": "completion"},
                   {"allow_ready": 1}, {"deadline": True}, {"operation": "delete_branch"},
                   {"ci_source": {**request()["ci_source"], "event": "push"}},
                   {"ci_source": {**request()["ci_source"], "required_jobs": ["test", "test"]}},
                   {"extra": "ignored?"}, {"version": True}]
        for change in changes:
            with self.subTest(change=change):
                result = self.execute({**request(), **change})
                self.assertEqual(result["status"], "blocked")
                self.assertEqual(result["effect"], "none")
        for raw in (b"", b"{}{}", b"{", b"[]", b'"hello"', b"\xff", b"NaN",
                    b'{"version":1,"version":1}', b" " * (adapter.MAX_INPUT + 1)):
            self.assertEqual(self.execute(raw=raw)["status"], "blocked")
        self.assertEqual(self.fixture.calls, [])

    def test_pending_or_non_success_ci_never_merges(self):
        for conclusion in (None, "failure", "cancelled", "timed_out", "skipped", "neutral"):
            with self.subTest(conclusion=conclusion):
                self.fixture = Fixture()
                for row in (self.fixture.run, self.fixture.suite, self.fixture.job, self.fixture.check):
                    row.update(status="in_progress" if conclusion is None else "completed", conclusion=conclusion)
                self.fixture.gql_pr["mergeStateStatus"] = "UNSTABLE"
                result = self.execute(request("merge"))
                self.assertEqual(result["status"], "waiting_ci", result)
                self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        self.fixture.jobs = []
        self.assertEqual(self.execute(request("merge"))["status"], "waiting_ci")
        self.assertEqual(self.fixture.writes(), [])

    def test_ready_can_trigger_draft_gated_ci_but_requires_new_preflight_before_merge(self):
        self.fixture.draft()
        self.fixture.ci_pending()
        before = self.execute()
        self.assertEqual(before["status"], "waiting_ci")
        self.assertTrue(before["target"]["draft"])
        self.assertEqual(self.fixture.writes(), [])
        result = self.execute({**request("ready"), "allow_ready": True})
        self.assertEqual((result["status"], result["effect"]), ("ready_confirmed", "ready_confirmed"), result)
        self.assertTrue(result["target"]["draft"])  # The saved preflight preceded ready.
        self.assertEqual(result["ci_observation"]["run"]["conclusion"], None)
        self.assertEqual(len(self.fixture.writes()), 1)
        self.assertEqual(fields(self.fixture.writes()[0]), {"query": adapter.READY_MUTATION, "id": "PR_fixed_13"})
        previous_reads = len(self.fixture.calls)
        after = self.execute(request("merge"))
        self.assertEqual(after["status"], "waiting_ci")
        self.assertGreater(len(self.fixture.calls), previous_reads + 5)
        self.assertEqual(len(self.fixture.writes()), 1)

    def test_draft_cannot_merge_and_already_ready_does_not_repeat_mutation(self):
        self.fixture.draft()
        result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "draft_requires_ready")
        self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        result = self.execute({**request("ready"), "allow_ready": True})
        self.assertEqual(result["error_code"], "already_ready_requires_observation")
        self.assertEqual(self.fixture.writes(), [])

    def test_source_forgery_or_ambiguous_identity_blocks_ready_and_merge(self):
        changes = [("run", "workflow_id", 20), ("run", "event", "push"), ("run", "head_sha", OTHER),
                   ("check", "app", {"id": 900}), ("check", "check_suite", {"id": 99}),
                   ("check", "name", "same-looking-test"), ("job", "run_attempt", 1),
                   ("job", "check_run_url", "https://evil.invalid/check-runs/29"),
                   ("pr", "node_id", "PR_other"), ("pr", "id", 14)]
        for operation in ("merge", "ready"):
            for row, key, value in changes:
                with self.subTest(operation=operation, row=row, key=key):
                    self.fixture = Fixture()
                    if operation == "ready":
                        self.fixture.draft()
                    getattr(self.fixture, row)[key] = value
                    result = self.execute({**request(operation), "allow_ready": True})
                    self.assertFalse(result["complete"], result)
                    self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        self.fixture.pr["head"]["repo"]["id"] = 12
        self.assertEqual(self.execute(request("merge"))["error_code"], "source_ambiguous")
        self.assertEqual(self.fixture.writes(), [])

    def test_newer_run_or_attempt_during_fresh_source_read_blocks_effect(self):
        for change in ({"run_attempt": 3}, {"id": 22, "run_number": 6}):
            with self.subTest(change=change):
                self.fixture = Fixture()
                newer = {**self.fixture.run, **change}
                self.fixture.hook = lambda endpoint, count: response({"total_count": 1, "workflow_runs": [newer]}) \
                    if endpoint == fixtures.LIST_ENDPOINT and count == 2 else None
                result = self.execute(request("merge"))
                self.assertEqual(result["error_code"], "source_changed", result)
                self.assertEqual(self.fixture.writes(), [])

    def test_final_target_head_base_node_or_stack_drift_prevents_write(self):
        changes = [{"headRefOid": OTHER}, {"baseRefName": "other"}, {"baseRefOid": OTHER},
                   {"id": "PR_other"}, {"stack": {"id": "stack-1"}}, {"stackEntry": {"id": "entry-1"}},
                   {"isMergeQueueEnabled": True}, {"autoMergeRequest": {"enabledAt": "now"}},
                   {"mergeStateStatus": "BLOCKED"}]
        for change in changes:
            with self.subTest(change=change):
                self.fixture = Fixture()
                def hook(method, endpoint, values, count):
                    if values.get("query") == adapter.PREFLIGHT_QUERY and count == 2:
                        changed = copy.deepcopy(self.fixture.gql)
                        changed["data"]["repository"]["pullRequest"].update(change)
                        return response(changed)
                    return None
                self.fixture.merge_hook = hook
                result = self.execute(request("merge"))
                self.assertFalse(result["complete"], result)
                self.assertEqual(self.fixture.writes(), [])

    def test_current_stack_queue_automerge_conflicts_unknown_and_incomplete_reads_block(self):
        changes = [{"stack": {"id": "one-entry-stack"}}, {"stackEntry": {"id": "entry"}},
                   {"mergeQueue": {"id": "queue"}}, {"mergeQueueEntry": {"id": "entry"}},
                   {"isInMergeQueue": True}, {"isMergeQueueEnabled": True},
                   {"autoMergeRequest": {"enabledAt": "now"}}, {"mergeable": "CONFLICTING"},
                   {"mergeable": "UNKNOWN"}, {"mergeStateStatus": "UNKNOWN"},
                   {"mergeStateStatus": "BLOCKED"}, {"mergeStateStatus": "BEHIND"},
                   {"mergeStateStatus": "UNSTABLE"}]
        for change in changes:
            with self.subTest(change=change):
                self.fixture = Fixture()
                self.fixture.gql_pr.update(change)
                result = self.execute(request("merge"))
                self.assertFalse(result["complete"], result)
                self.assertEqual(self.fixture.writes(), [])
        for missing in ("stack", "stackEntry", "mergeQueue", "isInMergeQueue", "autoMergeRequest"):
            self.fixture = Fixture()
            del self.fixture.gql_pr[missing]
            self.assertFalse(self.execute(request("merge"))["complete"])
            self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        self.fixture.gql["errors"] = [{"message": "private server context"}]
        result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "graphql_read_incomplete")
        self.assertNotIn("private", json.dumps(result))

    def test_protected_repo_not_reimplemented_as_rules_engine_and_delete_setting_is_read_only(self):
        self.fixture.repo["security_and_analysis"] = {"irrelevant": {"status": "enabled"}}
        self.fixture.pr["base"]["repo"]["protected"] = True
        result = self.execute(request("merge"))
        self.assertEqual(result["status"], "accepted")
        self.assertTrue(result["target"]["delete_branch_on_merge"])
        self.assertEqual(len(self.fixture.writes()), 1)
        self.fixture = Fixture()
        del self.fixture.repo["delete_branch_on_merge"]
        result = self.execute()
        self.assertEqual(result["status"], "preflight_ready")
        self.assertIsNone(result["target"]["delete_branch_on_merge"])

    def test_200_merged_or_enqueued_are_external_and_not_our_completion(self):
        for state in ("merged", "enqueued"):
            with self.subTest(state=state):
                self.fixture = Fixture()
                self.fixture.merge_response = response({"status": state, "details": {"sha": OTHER}})
                result = self.execute(request("merge"))
                self.assertEqual(result["status"], "externally_merged" if state == "merged" else "enqueued")
                self.assertNotEqual(result["effect"], "merge_confirmed")
                self.assertIsNone(result["async_request"])
                self.assertEqual(len(self.fixture.writes()), 1)

    def test_matching_409_records_external_request_and_never_attributes_it_to_relay(self):
        self.fixture.merge_response = response(pending(), 409)
        result = self.execute(request("merge"))
        self.assertEqual(result["status"], "pending", result)
        self.assertEqual(result["async_request"], accepted(provenance="external_unknown"))
        self.assertEqual(result["effect"], "none")
        self.fixture.poll_response = response({"status": "merged", "details": {"sha": OTHER}})
        result = self.execute({**request("reconcile"), "async_request": result["async_request"]})
        self.assertEqual((result["status"], result["effect"]), ("externally_merged", "externally_merged"))
        self.assertEqual(len(self.fixture.writes()), 1)

    def test_409_option_mismatch_not_adopted_and_no_retry(self):
        for change in ({"expected_head_sha": OTHER}, {"merge_method": "merge"},
                       {"merge_action": "default"}, {"merge_action": "merge_queue"}, {"bypass_rules": True}):
            with self.subTest(change=change):
                self.fixture = Fixture()
                value = pending()
                value["details"].update(change)
                self.fixture.merge_response = response(value, 409)
                result = self.execute(request("merge"))
                self.assertEqual(result["status"], "blocked")
                self.assertEqual(result["effect"], "none")
                self.assertEqual(result["error_code"], "async_request_mismatch")
                self.assertIsNone(result["async_request"])
                self.assertEqual(len(self.fixture.writes()), 1)

    def test_missing_uuid_lost_truncated_unknown_or_wrong_202_output_is_effect_unknown(self):
        missing = pending()
        del missing["details"]["uuid"]
        bad_options = pending()
        bad_options["details"]["bypass_rules"] = True
        for reply in (response(missing, 202), response(missing, 409), response(bad_options, 202), (1, b"lost secret output"),
                      (0, b"HTTP/2.0 202 Accepted\nContent-Type: application/json\n\n{"),
                      response({"status": "something_new", "details": {}}, 202),
                      response({"status": "merged", "details": {"sha": OTHER}}, 202),
                      response(pending(), 200), response({}, 503),
                      (0, b"x" * (adapter.MAX_RESPONSE + 1))):
            with self.subTest(reply=str(reply)[:80]):
                self.fixture = Fixture()
                self.fixture.merge_response = reply
                result = self.execute(request("merge"))
                self.assertEqual((result["status"], result["effect"]), ("effect_unknown", "unknown"), result)
                self.assertFalse(result["complete"])
                self.assertIsNone(result["async_request"])
                self.assertEqual(len(self.fixture.writes()), 1)
                self.assertNotIn("secret", json.dumps(result))

    def test_ready_lost_or_unverified_response_is_unknown_and_is_never_retried(self):
        for reply in ((1, b"lost"), response({"errors": [{"message": "no result"}]}),
                      response({"data": {"markPullRequestReadyForReview": {"pullRequest": {
                          "id": "PR_other", "fullDatabaseId": "13", "number": 7, "isDraft": False}}}})):
            self.fixture = Fixture()
            self.fixture.draft()
            self.fixture.merge_hook = lambda method, endpoint, values, count: reply \
                if values.get("query") == adapter.READY_MUTATION else None
            result = self.execute({**request("ready"), "allow_ready": True})
            self.assertEqual(result["status"], "effect_unknown", result)
            self.assertEqual(len(self.fixture.writes()), 1)

    def test_reconcile_pending_failed_merged_and_enqueued_only_reads_pinned_uuid(self):
        for state in ("pending", "failed", "merged", "enqueued"):
            with self.subTest(state=state):
                self.fixture = Fixture()
                body = pending() if state == "pending" else {
                    "status": state, "details": {"sha": OTHER, "message": "GitHub rejected protected rules"}}
                self.fixture.poll_response = response(body)
                result = self.execute({**request("reconcile"), "async_request": accepted(), "deadline": NOW - 10},
                                      {k: v for k, v in self.env.items() if k != "RELAY_MERGE_WRITE"})
                self.assertEqual(result["status"], state, result)
                self.assertTrue(result["complete"])
                self.assertEqual(result["async_request"], accepted())
                self.assertIsNone(result["target"])
                self.assertIsNone(result["ci_observation"])
                self.assertEqual(self.fixture.writes(), [])
                self.assertEqual([(c[5], c[-1]) for c in self.fixture.calls], [("GET", f"{MERGE}/{UUID}")])
                self.assertNotIn("protected rules", json.dumps(result))
                if state == "merged":
                    self.assertEqual(result["effect"], "merge_confirmed")

    def test_read_404_may_be_result_expiry_and_never_causes_retransmission(self):
        self.fixture.poll_response = response({"message": "Not found"}, 404)
        result = self.execute({**request("reconcile"), "async_request": accepted(), "deadline": NOW - 100})
        self.assertEqual(result["status"], "effect_unknown")
        self.assertEqual(result["error_code"], "result_expired_or_unavailable")
        self.assertEqual(result["async_request"], accepted())
        self.assertEqual(self.fixture.writes(), [])

    def test_reconcile_without_uuid_can_only_observe_external_merge_or_remain_unknown(self):
        value = {**request("reconcile"), "deadline": NOW - 100}
        result = self.execute(value)
        self.assertEqual(result["status"], "effect_unknown")
        self.fixture.pr.update(state="closed", merged=True)
        result = self.execute(value)
        self.assertEqual((result["status"], result["effect"]), ("externally_merged", "externally_merged"))
        self.assertEqual(self.fixture.writes(), [])
        self.assertTrue(all(c[5] == "GET" and c[-1] == PR for c in self.fixture.calls))

    def test_poll_mismatched_uuid_or_options_keeps_original_receipt_unknown(self):
        for change in ({"uuid": "730b9d5e-3f2a-4f7e-8b0c-2d5f9a8c1e42"}, {"bypass_rules": True},
                       {"expected_head_sha": OTHER}, {"merge_action": "merge_queue"}):
            self.fixture = Fixture()
            body = pending()
            body["details"].update(change)
            self.fixture.poll_response = response(body)
            result = self.execute({**request("reconcile"), "async_request": accepted()})
            self.assertEqual(result["status"], "effect_unknown")
            self.assertEqual(result["async_request"], accepted())
            self.assertEqual(self.fixture.writes(), [])

    def test_deadline_is_last_dispatch_gate_but_late_response_is_retained(self):
        # Expiration during the final preflight prevents mutation dispatch.
        def expire(method, endpoint, values, count):
            if values.get("query") == adapter.PREFLIGHT_QUERY and count == 2:
                adapter.time.time.return_value = NOW + 3601
            return None
        self.fixture.merge_hook = expire
        result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "authorization_expired")
        self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        # Expiration after dispatch cannot unsend the accepted request.
        def late(method, endpoint, values, count):
            if endpoint == MERGE:
                adapter.time.time.return_value = NOW + 3601
            return None
        self.fixture.merge_hook = late
        result = self.execute(request("merge"))
        self.assertEqual(result["status"], "accepted", result)
        self.assertEqual(result["async_request"], accepted())
        self.assertEqual(len(self.fixture.writes()), 1)

    def test_auth_rate_unsupported_and_rule_rejections_are_bounded_no_bypass(self):
        cases = [(401, {}, "auth_required"), (403, {}, "permission_denied"),
                 (404, {}, "async_api_unavailable"), (400, {}, "github_merge_rejected"),
                 (422, {}, "github_merge_rejected"), (429, {}, "rate_limited"),
                 (403, {"Retry-After": "30"}, "rate_limited")]
        for status, headers, code in cases:
            self.fixture = Fixture()
            self.fixture.merge_response = response({"message": "secret server context"}, status, headers)
            result = self.execute(request("merge"))
            self.assertEqual(result["error_code"], code, result)
            self.assertEqual(result["effect"], "none")
            self.assertNotEqual(result["status"], "accepted")
            self.assertEqual(len(self.fixture.writes()), 1)
            self.assertNotIn("secret", json.dumps(result))

    def test_read_permission_auth_rate_and_graphql_failure_prevent_mutation(self):
        for reply in (response({}, 401), response({}, 403), response({}, 404),
                      response({}, 429), response({}, 503), (4, b"secret authentication context")):
            self.fixture = Fixture()
            self.fixture.merge_hook = lambda method, endpoint, values, count: reply
            result = self.execute(request("merge"))
            self.assertFalse(result["complete"])
            self.assertEqual(result["effect"], "none")
            self.assertEqual(self.fixture.writes(), [])
            self.assertNotIn("secret", json.dumps(result))

    def test_transient_ci_read_failure_stays_read_only_and_transient(self):
        self.fixture.hook = lambda endpoint, count: response({}, 429) \
            if endpoint == fixtures.LIST_ENDPOINT else None
        result = self.execute(request("merge"))
        self.assertEqual(result["status"], "transient_error", result)
        self.assertEqual(result["error_code"], "rate_limited")
        self.assertEqual(result["effect"], "none")
        self.assertEqual(self.fixture.writes(), [])

    def test_budgets_fail_closed_and_do_not_cut_prefix_into_evidence(self):
        with patch.object(adapter, "MAX_REQUESTS", 3):
            result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "request_limit")
        self.assertEqual(len(self.fixture.calls), 3)
        self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        with patch.object(adapter, "MAX_TOTAL", 200):
            result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "response_limit")
        self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        with patch.object(adapter.time, "monotonic", side_effect=[0, 31]):
            result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "request_timeout")
        self.assertEqual(self.fixture.calls, [])

    def test_credentials_inherited_not_changed_or_logged_and_debug_suppressed(self):
        env = {**self.env, "GH_TOKEN": "secret-token", "GITHUB_TOKEN": "second-secret",
               "GH_CONFIG_DIR": "/host/existing-auth", "GH_DEBUG": "api", "GH_HOST": "evil.invalid"}
        result = self.execute(request("merge"), env)
        self.assertEqual(result["status"], "accepted")
        self.assertNotIn("secret", json.dumps(result))
        for seen in self.fixture.environments:
            self.assertEqual(seen["GH_HOST"], "github.com")
            self.assertEqual(seen["GH_PROMPT_DISABLED"], "1")
            self.assertEqual(seen["GH_TOKEN"], "secret-token")
            self.assertEqual(seen["GITHUB_TOKEN"], "second-secret")
            self.assertEqual(seen["GH_CONFIG_DIR"], "/host/existing-auth")
            self.assertNotIn("GH_DEBUG", seen)

    def test_transport_refuses_unapproved_routes_queries_options_and_write_operations(self):
        api = adapter.GitHub(adapter.parse_request(request("merge")), self.env)
        invalid = [("GET", "https://api.github.com/" + PR, None), ("GET", PR + "?x=y", None),
                   ("GET", f"{PREFIX}/pulls/8", None), ("GET", f"{MERGE}/{UUID}", None),
                   ("GET", f"{PREFIX}/actions/runs/21/rerun", None),
                   ("PUT", f"{PR}/merge", adapter.options(request())),
                   ("PUT", MERGE, {**adapter.options(request()), "bypass_rules": True}),
                   ("PUT", MERGE, {**adapter.options(request()), "merge_action": "default"}),
                   ("PUT", MERGE, {**adapter.options(request()), "sha": OTHER}),
                   ("POST", "graphql", {"query": "mutation {mergePullRequest {id}}"}),
                   ("POST", "graphql", {"query": adapter.READY_MUTATION, "id": "PR_fixed_13"}),
                   ("DELETE", f"{PREFIX}/git/refs/heads/main", None),
                   ("PATCH", PREFIX, {"delete_branch_on_merge": False})]
        with patch.object(adapter, "capture") as captured:
            for method, endpoint, payload in invalid:
                with self.subTest(method=method, endpoint=endpoint), self.assertRaises(adapter.Error):
                    api.request_json(method, endpoint, payload)
        captured.assert_not_called()
        for operation in ("preflight", "reconcile"):
            api = adapter.GitHub(adapter.parse_request(request(operation)), self.env)
            with patch.object(adapter, "capture") as captured, self.assertRaises(adapter.Error):
                api.request_json("PUT", MERGE, adapter.options(request()))
            captured.assert_not_called()

    def test_capture_inherits_host_lifetime_without_shell_stderr_or_stdin(self):
        child = fixtures.process(b"response")
        with patch.object(subprocess, "Popen", return_value=child) as process:
            self.assertEqual(adapter.capture(["/host/bin/gh", "api"], {"GH_TOKEN": "secret"}), (0, b"response"))
        self.assertEqual(process.call_args.kwargs, {"stdin": subprocess.DEVNULL, "stdout": subprocess.PIPE,
                         "stderr": subprocess.DEVNULL, "env": {"GH_TOKEN": "secret"}, "shell": False})

    def test_mutations_require_host_gate_but_readonly_operations_do_not(self):
        for operation in ("ready", "merge"):
            self.fixture = Fixture()
            if operation == "ready":
                self.fixture.draft()
            result = self.execute({**request(operation), "allow_ready": True}, auto_gate=False)
            self.assertEqual(result["error_code"], "write_gate_invalid", result)
            self.assertEqual(result["effect"], "none")
            self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        self.assertEqual(self.execute(auto_gate=False)["status"], "preflight_ready")
        self.assertEqual(self.execute({**request("reconcile"), "async_request": accepted()},
                                     auto_gate=False)["status"], "pending")

    def test_gate_rejects_wrong_binding_identity_mode_owner_or_malformed_json(self):
        value = request("merge")
        changes = [{"version": True}, {"authorization_id": 42}, {"authorization_id": True},
                   {"attempt": 2}, {"phase": "ready"}, {"consent_sha256": "e" * 64},
                   {"deadline": NOW + 4000}, {"revoked": 0}, {"write_started": 0}, {"extra": True}]
        for change in changes:
            with self.subTest(change=change):
                self.fixture = Fixture()
                env = {**self.env, **self.make_gate(value, **change)}
                result = self.execute(value, env, auto_gate=False)
                self.assertEqual(result["error_code"], "write_gate_invalid", result)
                self.assertEqual(result["effect"], "none")
                self.assertEqual(self.fixture.writes(), [])
        for permission in (0o644, 0o400, 0o660, 0o1600):
            self.fixture = Fixture()
            env = {**self.env, **self.make_gate(value)}
            self.gate_path.chmod(permission)
            result = self.execute(value, env, auto_gate=False)
            self.assertEqual(result["error_code"], "write_gate_invalid", result)
            self.assertEqual(self.fixture.writes(), [])
        for key in ("RELAY_MERGE_GATE_DEVICE", "RELAY_MERGE_GATE_INODE"):
            self.fixture = Fixture()
            env = {**self.env, **self.make_gate(value)}
            env[key] = str(int(env[key]) + 1)
            self.assertEqual(self.execute(value, env, auto_gate=False)["error_code"], "write_gate_invalid")
            self.assertEqual(self.fixture.writes(), [])
        for raw in (b"{", b"{}", b"{}{}", b'{"version":1,"version":1}', b" " * 4097):
            self.fixture = Fixture()
            env = {**self.env, **self.make_gate(value)}
            self.gate_path.write_bytes(raw)
            self.assertEqual(self.execute(value, env, auto_gate=False)["error_code"], "write_gate_invalid")
            self.assertEqual(self.fixture.writes(), [])
        self.fixture = Fixture()
        env = {**self.env, **self.make_gate(value)}
        with patch.object(adapter.os, "geteuid", return_value=os.geteuid() + 1):
            self.assertEqual(self.execute(value, env, auto_gate=False)["error_code"], "write_gate_invalid")
        self.assertEqual(self.fixture.writes(), [])

    def test_gate_rejects_missing_replaced_symlink_or_hardlinked_file(self):
        value = request("merge")
        for change in ("missing", "replaced", "symlink", "hardlink"):
            with self.subTest(change=change):
                self.fixture = Fixture()
                env = {**self.env, **self.make_gate(value)}
                other = self.gate_path.with_name("old-" + change)
                if change == "hardlink":
                    os.link(self.gate_path, other)
                else:
                    self.gate_path.rename(other)
                    if change == "replaced":
                        self.make_gate(value)
                    elif change == "symlink":
                        self.gate_path.symlink_to(other)
                result = self.execute(value, env, auto_gate=False)
                self.assertEqual(result["error_code"], "write_gate_invalid", result)
                self.assertEqual(self.fixture.writes(), [])
                if self.gate_path.exists() or self.gate_path.is_symlink():
                    self.gate_path.unlink()
                other.unlink()

    def test_gate_blocks_revoked_or_previously_dispatched_attempt_without_rewrite(self):
        for change, code in (({"revoked": True}, "authorization_revoked"),
                             ({"write_started": True}, "write_already_attempted")):
            self.fixture = Fixture()
            value = request("merge")
            env = {**self.env, **self.make_gate(value, **change)}
            before = self.gate_path.read_bytes()
            result = self.execute(value, env, auto_gate=False)
            self.assertEqual(result["error_code"], code, result)
            self.assertEqual(result["effect"], "none")
            self.assertEqual(self.fixture.writes(), [])
            self.assertEqual(self.gate_path.read_bytes(), before)

    def test_revoke_during_last_preflight_read_wins_before_ready_or_merge(self):
        for operation in ("ready", "merge"):
            self.fixture = Fixture()
            if operation == "ready":
                self.fixture.draft()
            def revoke(method, endpoint, values, count):
                if values.get("query") == adapter.PREFLIGHT_QUERY and count == 2:
                    with self.gate_path.open("r+") as gate:
                        # Preflight has no shared lock, so host revocation wins immediately.
                        fcntl.flock(gate, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        state = json.load(gate)
                        self.assertFalse(state["write_started"])
                        state["revoked"] = True
                        gate.seek(0)
                        json.dump(state, gate)
                        gate.truncate()
                        gate.flush()
                        os.fsync(gate.fileno())
                return None
            self.fixture.merge_hook = revoke
            result = self.execute({**request(operation), "allow_ready": True})
            self.assertEqual(result["error_code"], "authorization_revoked", result)
            self.assertEqual(result["effect"], "none")
            self.assertEqual(self.fixture.writes(), [])
            self.assertFalse(json.loads(self.gate_path.read_text())["write_started"])

    def test_dispatch_marks_gate_durably_and_holds_shared_lock_through_capture(self):
        requested, revoked = threading.Event(), threading.Event()
        failures = []
        def revoker():
            try:
                requested.wait(2)
                with self.gate_path.open("r+") as gate:
                    fcntl.flock(gate, fcntl.LOCK_EX)
                    state = json.load(gate)
                    self.assertTrue(state["write_started"])
                    state["revoked"] = True
                    gate.seek(0)
                    json.dump(state, gate)
                    gate.truncate()
                    gate.flush()
                    os.fsync(gate.fileno())
                    revoked.set()
            except BaseException as error:
                failures.append(error)
        worker = threading.Thread(target=revoker, daemon=True)
        worker.start()
        def during_capture(method, endpoint, values, count):
            if endpoint == MERGE:
                state = json.loads(self.gate_path.read_text())
                self.assertTrue(state["write_started"])
                with self.gate_path.open("r+") as gate:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(gate, fcntl.LOCK_EX | fcntl.LOCK_NB)
                requested.set()
                self.assertFalse(revoked.wait(0.05))
            return None
        self.fixture.merge_hook = during_capture
        result = self.execute(request("merge"))
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])
        self.assertTrue(revoked.is_set())
        self.assertEqual(result["status"], "accepted", result)
        self.assertEqual(len(self.fixture.writes()), 1)
        self.assertTrue(json.loads(self.gate_path.read_text())["revoked"])

    def test_waiting_for_exclusive_revoker_does_not_lock_preflight_or_allow_write(self):
        value = request("merge")
        env = {**self.env, **self.make_gate(value)}
        final_read, released = threading.Event(), threading.Event()
        failures = []
        gate = self.gate_path.open("r+")
        fcntl.flock(gate, fcntl.LOCK_EX)
        def release_after_preflight():
            try:
                self.assertTrue(final_read.wait(2))
                time.sleep(0.05)
                state = json.load(gate)
                state["revoked"] = True
                gate.seek(0)
                json.dump(state, gate)
                gate.truncate()
                gate.flush()
                os.fsync(gate.fileno())
            except BaseException as error:
                failures.append(error)
            finally:
                gate.close()
                released.set()
        worker = threading.Thread(target=release_after_preflight, daemon=True)
        worker.start()
        def saw_read(method, endpoint, values, count):
            if values.get("query") == adapter.PREFLIGHT_QUERY and count == 2:
                self.assertFalse(released.is_set())
                final_read.set()
            return None
        self.fixture.merge_hook = saw_read
        result = self.execute(value, env, auto_gate=False)
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])
        self.assertEqual(result["error_code"], "authorization_revoked", result)
        self.assertEqual(self.fixture.writes(), [])

    def test_gate_wait_is_bounded_and_rechecks_deadline_after_lock(self):
        value = request("merge")
        env = {**self.env, **self.make_gate(value)}
        api = adapter.GitHub(adapter.parse_request(value), env)
        with self.gate_path.open("r+") as gate:
            fcntl.flock(gate, fcntl.LOCK_EX)
            with patch.object(adapter.time, "time", return_value=NOW), \
                    patch.object(adapter.time, "monotonic", side_effect=[api.started, api.started + 31]), \
                    patch.object(adapter.time, "sleep"), \
                    self.assertRaisesRegex(adapter.Error, "write_gate_timeout"):
                with adapter.write_gate(api):
                    self.fail("locked gate must not dispatch")
            with patch.object(adapter.time, "time", side_effect=[NOW, NOW + 3601]), \
                    patch.object(adapter.time, "sleep"), \
                    self.assertRaisesRegex(adapter.Error, "authorization_expired"):
                with adapter.write_gate(api):
                    self.fail("expiration while waiting must not dispatch")
        self.assertFalse(json.loads(self.gate_path.read_text())["write_started"])
        with patch.object(adapter.time, "time", side_effect=[NOW, NOW + 3601]), \
                self.assertRaisesRegex(adapter.Error, "authorization_expired"):
            with adapter.write_gate(api):
                self.fail("expired gate must not dispatch")
        self.assertFalse(json.loads(self.gate_path.read_text())["write_started"])

    def test_gate_fsync_failure_is_no_write_and_capture_failure_stays_unknown(self):
        with patch.object(adapter.os, "fsync", side_effect=OSError("private host error")):
            result = self.execute(request("merge"))
        self.assertEqual(result["error_code"], "write_gate_invalid")
        self.assertEqual(result["effect"], "none")
        self.assertEqual(self.fixture.writes(), [])
        self.assertNotIn("private", json.dumps(result))
        self.fixture = Fixture()
        attempted = []
        def lost(method, endpoint, values, count):
            if endpoint == MERGE:
                attempted.append(endpoint)
                raise OSError("secret transport failure")
            return None
        self.fixture.merge_hook = lost
        result = self.execute(request("merge"))
        self.assertEqual((result["status"], result["effect"]), ("effect_unknown", "unknown"))
        self.assertEqual(attempted, [MERGE])
        self.assertTrue(json.loads(self.gate_path.read_text())["write_started"])
        self.assertNotIn("secret", json.dumps(result))
        with self.gate_path.open("r+") as gate:
            fcntl.flock(gate, fcntl.LOCK_EX | fcntl.LOCK_NB)  # No abandoned SH lock.


if __name__ == "__main__":
    unittest.main()
