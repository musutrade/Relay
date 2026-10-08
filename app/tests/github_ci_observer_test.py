"""Offline exact-head GitHub Actions observer tests; never contact GitHub."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import MagicMock, patch


SPEC = importlib.util.spec_from_file_location(
    "github_ci_observer", Path(__file__).resolve().parents[2] / "examples/github-ci-observe.py")
observer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(observer)
HEAD = "a" * 40
BASE = "b" * 40
OTHER = "c" * 40
REPO = "example/project"
PREFIX = f"repos/{REPO}"
PR_ENDPOINT = f"{PREFIX}/pulls/7"
LIST_ENDPOINT = f"{PREFIX}/actions/workflows/19/runs?head_sha={HEAD}&event=pull_request&per_page=100&page=1"
RUN_ENDPOINT = f"{PREFIX}/actions/runs/21"
SUITE_ENDPOINT = f"{PREFIX}/check-suites/23"
JOBS_ENDPOINT = f"{PREFIX}/actions/runs/21/attempts/2/jobs?per_page=100&page=1"
CHECK_ENDPOINT = f"{PREFIX}/check-runs/29"


def request():
    return {"version": 1, "repository": REPO, "pr_number": 7,
            "pr_url": f"https://github.com/{REPO}/pull/7", "head_sha": HEAD,
            "head_branch": "relay/task-1-g1", "base_branch": "main",
            "workflow_id": 19, "app_id": 15368, "event": "pull_request",
            "required_jobs": ["test"], "observed_repository_id": None,
            "observed_pr_id": None, "observed_base_sha": None}


def response(data, code=200, headers=None):
    headers = {"Content-Type": "application/json; charset=utf-8", **(headers or {})}
    raw = f"HTTP/2.0 {code} Status\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers.items())
    return (0 if code == 200 else 1), (raw + "\r\n" + json.dumps(data)).encode()


def process(raw, code=0):
    child = MagicMock()
    child.__enter__.return_value = child
    child.stdout = io.BytesIO(raw)
    child.wait.return_value = code
    return child


class Fixture:
    def __init__(self):
        repo = {"id": 11, "full_name": REPO, "fork": False}
        self.pr = {"id": 13, "number": 7, "html_url": f"https://github.com/{REPO}/pull/7",
                   "state": "open", "merged": False, "draft": True, "mergeable": True,
                   "head": {"sha": HEAD, "ref": "relay/task-1-g1", "repo": copy.deepcopy(repo)},
                   "base": {"sha": BASE, "ref": "main", "repo": copy.deepcopy(repo)}}
        link = {"id": 13, "number": 7, "head": copy.deepcopy(self.pr["head"]),
                "base": copy.deepcopy(self.pr["base"])}
        self.run = {"id": 21, "run_number": 5, "run_attempt": 2, "workflow_id": 19,
                    "event": "pull_request", "head_sha": HEAD, "head_branch": "relay/task-1-g1",
                    "repository": copy.deepcopy(repo), "head_repository": copy.deepcopy(repo),
                    "check_suite_id": 23, "status": "completed", "conclusion": "success",
                    "pull_requests": [copy.deepcopy(link)]}
        self.suite = {"id": 23, "app": {"id": 15368}, "head_sha": HEAD,
                      "head_branch": "relay/task-1-g1", "repository": copy.deepcopy(repo),
                      "pull_requests": [copy.deepcopy(link)], "status": "completed", "conclusion": "success"}
        self.job = {"id": 27, "run_id": 21, "run_attempt": 2, "head_sha": HEAD,
                    "name": "test", "status": "completed", "conclusion": "success",
                    "check_run_url": f"https://api.github.com/{PREFIX}/check-runs/29"}
        self.check = {"id": 29, "name": "test", "head_sha": HEAD, "app": {"id": 15368},
                      "check_suite": {"id": 23}, "status": "completed", "conclusion": "success"}
        self.runs = [self.run]
        self.jobs = [self.job]
        self.calls = []
        self.environments = []
        self.hook = None

    def capture(self, command, env):
        endpoint = command[-1]
        self.calls.append(command)
        self.environments.append(env)
        if self.hook:
            override = self.hook(endpoint, sum(c[-1] == endpoint for c in self.calls))
            if override is not None:
                return override
        values = {PR_ENDPOINT: self.pr, LIST_ENDPOINT: {"total_count": len(self.runs), "workflow_runs": self.runs},
                  RUN_ENDPOINT: self.run, SUITE_ENDPOINT: self.suite,
                  JOBS_ENDPOINT: {"total_count": len(self.jobs), "jobs": self.jobs}, CHECK_ENDPOINT: self.check}
        if endpoint not in values:
            raise AssertionError(f"unexpected endpoint: {endpoint}")
        return response(values[endpoint])


class ObserverTests(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture()
        self.request = request()
        self.env = {"RELAY_CI_OBSERVE": "1", "RELAY_GH_PROGRAM": "/host/bin/gh"}

    def execute(self, request_value=None, env=None, raw=None):
        raw = json.dumps(self.request if request_value is None else request_value).encode() if raw is None else raw
        output = io.StringIO()
        with patch.object(observer, "capture", side_effect=self.fixture.capture), \
                patch.object(subprocess, "Popen") as popen, contextlib.redirect_stdout(output):
            self.assertEqual(observer.main(self.env if env is None else env, io.BytesIO(raw)), 0)
        popen.assert_not_called()
        result = json.loads(output.getvalue())
        self.assertLessEqual(len(output.getvalue().encode()), observer.MAX_OUTPUT + 1)
        self.assertEqual(result["remote_merge_eligibility"], "not_established")
        return result

    def assert_blocked(self, code=None):
        result = self.execute()
        self.assertEqual(result["observation"], "blocked", result)
        self.assertFalse(result["complete"])
        if code:
            self.assertEqual(result["error_code"], code, result)
        self.assertIsNone(result["run"])
        return result

    def test_verified_success_has_exact_frozen_protocol(self):
        result = self.execute()
        self.assertEqual(set(result), {"version", "observation", "complete", "repository", "pull_request",
                                       "run", "error_code", "detail", "remote_merge_eligibility"})
        self.assertEqual(result["observation"], "ok")
        self.assertTrue(result["complete"])
        self.assertIsNone(result["error_code"])
        self.assertEqual(result["repository"], {"id": 11, "full_name": REPO})
        self.assertEqual(result["pull_request"], {"id": 13, "number": 7,
                         "url": self.request["pr_url"], "state": "open", "merged": False,
                         "draft": True, "head_sha": HEAD, "head_ref": "relay/task-1-g1",
                         "head_repository_id": 11, "base_ref": "main", "base_sha": BASE,
                         "base_repository_id": 11, "mergeable": True})
        self.assertEqual(result["run"], {"id": 21, "run_number": 5, "run_attempt": 2,
                         "workflow_id": 19, "event": "pull_request", "head_sha": HEAD,
                         "head_repository_id": 11, "check_suite_id": 23, "app_id": 15368,
                         "status": "completed", "conclusion": "success", "pull_request_ids": [13],
                         "jobs": [{"id": 27, "check_run_id": 29, "name": "test", "head_sha": HEAD,
                                   "check_suite_id": 23, "app_id": 15368,
                                   "status": "completed", "conclusion": "success"}]})
        self.assertEqual([c[-1] for c in self.fixture.calls], [PR_ENDPOINT, LIST_ENDPOINT, SUITE_ENDPOINT,
                         JOBS_ENDPOINT, CHECK_ENDPOINT, RUN_ENDPOINT, LIST_ENDPOINT, PR_ENDPOINT])

    def test_only_explicit_bounded_fixed_get_commands_and_credentials_preserved(self):
        env = {**self.env, "GH_TOKEN": "secret-do-not-print", "GITHUB_TOKEN": "also-secret",
               "GH_CONFIG_DIR": "/host/auth", "GH_DEBUG": "api", "GH_HOST": "attacker.invalid"}
        result = self.execute(env=env)
        self.assertEqual(result["observation"], "ok")
        self.assertNotIn("secret", json.dumps(result))
        for command, captured_env in zip(self.fixture.calls, self.fixture.environments):
            self.assertEqual(command[:6], ["/host/bin/gh", "api", "--hostname", "github.com", "--method", "GET"])
            for forbidden in ("--paginate", "--watch", "POST", "PATCH", "PUT", "DELETE", "pr", "merge", "rerun"):
                self.assertNotIn(forbidden, command)
            self.assertEqual(captured_env["GH_HOST"], "github.com")
            self.assertEqual(captured_env["GH_PROMPT_DISABLED"], "1")
            self.assertNotIn("GH_DEBUG", captured_env)
            self.assertEqual(captured_env["GH_TOKEN"], env["GH_TOKEN"])
            self.assertEqual(captured_env["GITHUB_TOKEN"], env["GITHUB_TOKEN"])
            self.assertEqual(captured_env["GH_CONFIG_DIR"], env["GH_CONFIG_DIR"])

    def test_no_subprocess_default_or_invalid_configuration(self):
        for env, code in [({}, "observation_disabled"), ({**self.env, "RELAY_CI_OBSERVE": "0"}, "observation_disabled"),
                          ({"RELAY_CI_OBSERVE": "1"}, "configuration_invalid"),
                          ({**self.env, "RELAY_GH_PROGRAM": "gh"}, "configuration_invalid")]:
            with self.subTest(env=env):
                result = self.execute(env=env)
                self.assertEqual(result["error_code"], code)
                self.assertEqual(self.fixture.calls, [])

    def test_input_is_strict_and_never_routes_untrusted_fields(self):
        cases = [{"repository": "evil/../repo"}, {"repository": "https://github.com/example/project"},
                 {"repository": "example/project?foo=bar"}, {"pr_number": True}, {"pr_number": 0},
                 {"pr_url": "https://attacker.invalid/example/project/pull/7"}, {"head_sha": HEAD + "&x=y"},
                 {"head_branch": "../../main"}, {"base_branch": "main\n--method=POST"},
                 {"workflow_id": "19/runs"}, {"app_id": -1}, {"event": "push"},
                 {"required_jobs": []}, {"required_jobs": ["test", "test"]},
                 {"required_jobs": ["test\nsecret"]}, {"required_jobs": [" "]},
                 {"required_jobs": [str(n) for n in range(9)]}, {"version": 2}, {"version": True},
                 {"observed_repository_id": "11"}, {"observed_pr_id": True}, {"observed_base_sha": "short"},
                 {"unexpected": "ignored?"}]
        for change in cases:
            with self.subTest(change=change):
                result = self.execute({**self.request, **change})
                self.assertEqual(result["observation"], "blocked")
                self.assertFalse(result["complete"])
        for raw in (b"", b"{}{}", b"{", b"[]", b'"hello"', b"\xff", b"NaN",
                    b'{"version":1,"version":1}', b" " * (observer.MAX_INPUT + 1)):
            with self.subTest(raw=raw[:40]):
                self.assertEqual(self.execute(raw=raw)["observation"], "blocked")
        self.assertEqual(self.fixture.calls, [])

    def test_no_run_and_missing_required_jobs_stay_complete_pending_evidence(self):
        self.fixture.runs = []
        result = self.execute()
        self.assertEqual(result["observation"], "ok")
        self.assertIsNone(result["run"])
        self.assertEqual(len(self.fixture.calls), 4)
        self.fixture = Fixture()
        self.fixture.jobs = []
        result = self.execute()
        self.assertEqual(result["observation"], "ok")
        self.assertEqual(result["run"]["jobs"], [])

    def test_eight_required_jobs_fit_normal_fixed_request_budget(self):
        self.request["required_jobs"] = [f"job-{n}" for n in range(8)]
        self.fixture.jobs = [{**self.fixture.job, "id": 100 + n, "name": f"job-{n}",
                             "check_run_url": f"https://api.github.com/{PREFIX}/check-runs/{200 + n}"}
                            for n in range(8)]
        checks = {f"{PREFIX}/check-runs/{200 + n}": {**self.fixture.check, "id": 200 + n, "name": f"job-{n}"}
                  for n in range(8)}
        self.fixture.hook = lambda endpoint, _: response(checks[endpoint]) if endpoint in checks else None
        result = self.execute()
        self.assertEqual(result["observation"], "ok")
        self.assertEqual(len(result["run"]["jobs"]), 8)
        self.assertEqual(len(self.fixture.calls), 15)

    def test_maximum_pages_and_eight_required_jobs_fit_combined_request_budget(self):
        self.request["required_jobs"] = [f"job-{n}" for n in range(8)]
        self.fixture.run["run_number"] = 201
        runs = [{**self.fixture.run, "id": 1000 + n, "run_number": n + 1} for n in range(200)]
        runs.append(self.fixture.run)
        jobs = [{**self.fixture.job, "id": 100 + n, "name": f"job-{n}",
                 "check_run_url": f"https://api.github.com/{PREFIX}/check-runs/{200 + n}"}
                for n in range(8)]
        jobs.extend({**self.fixture.job, "id": 3000 + n, "name": f"unrequired-{n}"} for n in range(193))
        checks = {f"{PREFIX}/check-runs/{200 + n}": {**self.fixture.check, "id": 200 + n, "name": f"job-{n}"}
                  for n in range(8)}

        def hook(endpoint, _):
            if endpoint in checks:
                return response(checks[endpoint])
            for prefix, values, key in ((LIST_ENDPOINT.rsplit("page=", 1)[0] + "page=", runs, "workflow_runs"),
                                        (JOBS_ENDPOINT.rsplit("page=", 1)[0] + "page=", jobs, "jobs")):
                if endpoint.startswith(prefix):
                    page = int(endpoint[len(prefix):])
                    return response({"total_count": len(values), key: values[(page - 1) * 100:page * 100]})
            return None

        self.fixture.hook = hook
        result = self.execute()
        self.assertEqual(result["observation"], "ok")
        self.assertTrue(result["complete"])
        self.assertEqual(len(result["run"]["jobs"]), 8)
        self.assertEqual(len(self.fixture.calls), 21)
        self.assertLessEqual(len(self.fixture.calls), observer.MAX_REQUESTS)

    def test_actual_request_budget_exact_limit_and_one_beyond(self):
        self.assertEqual(observer.MAX_REQUESTS, 24)
        api = observer.GitHub(self.request, self.env)
        with patch.object(observer, "capture", side_effect=self.fixture.capture):
            for _ in range(24):
                api.get(PR_ENDPOINT)
            with self.assertRaisesRegex(observer.ObservationError, "request_limit"):
                api.get(PR_ENDPOINT)
        self.assertEqual(len(self.fixture.calls), 24)

    def test_only_success_can_be_a_pass_every_known_non_success_remains_evidence(self):
        for conclusion in sorted(observer.CONCLUSIONS):
            with self.subTest(conclusion=conclusion):
                self.fixture = Fixture()
                for value in (self.fixture.run, self.fixture.suite, self.fixture.job, self.fixture.check):
                    value["conclusion"] = conclusion
                result = self.execute()
                self.assertEqual(result["observation"], "ok")
                self.assertEqual(result["run"]["jobs"][0]["conclusion"], conclusion)
        for state in sorted(observer.PENDING):
            with self.subTest(state=state):
                self.fixture = Fixture()
                for value in (self.fixture.run, self.fixture.suite, self.fixture.job, self.fixture.check):
                    value.update(status=state, conclusion=None)
                result = self.execute()
                self.assertEqual(result["observation"], "ok")
                self.assertEqual(result["run"]["status"], state)
                self.assertIsNone(result["run"]["jobs"][0]["conclusion"])

    def test_unknown_inconsistent_or_malformed_statuses_are_not_passes(self):
        for target in ("run", "suite", "job", "check"):
            for change in ({"status": "unknown"}, {"status": []}, {"conclusion": "unknown"},
                           {"status": "completed", "conclusion": None},
                           {"status": "queued", "conclusion": "success"}):
                with self.subTest(target=target, change=change):
                    self.fixture = Fixture()
                    getattr(self.fixture, target).update(change)
                    self.assert_blocked("protocol_invalid")

    def test_wrong_producer_workflow_event_head_repository_pr_link_rejected(self):
        cases = [("run", {"workflow_id": 20}), ("run", {"event": "pull_request_target"}),
                 ("run", {"head_sha": OTHER}), ("run", {"head_branch": "other"}),
                 ("run", {"head_repository": {"id": 999, "full_name": REPO, "fork": False}}),
                 ("run", {"pull_requests": []}),
                 ("suite", {"app": {"id": 1}}), ("suite", {"head_sha": OTHER}),
                 ("suite", {"id": 24}), ("job", {"run_id": 22}), ("job", {"run_attempt": 1}),
                 ("job", {"head_sha": OTHER}), ("check", {"app": {"id": 1}}),
                 ("check", {"check_suite": {"id": 24}}), ("check", {"name": "else"}),
                 ("check", {"head_sha": OTHER}), ("check", {"id": 999})]
        for target, change in cases:
            with self.subTest(target=target, change=change):
                self.fixture = Fixture()
                getattr(self.fixture, target).update(change)
                self.assert_blocked()
        for side, change in (("head", {"sha": OTHER}), ("head", {"ref": "other"}),
                             ("base", {"ref": "else"}), ("base", {"repo": {"id": 999}})):
            self.fixture = Fixture()
            self.fixture.run["pull_requests"][0][side].update(change)
            self.assert_blocked()

    def test_forks_retargets_and_numeric_identity_reuse_fail_closed(self):
        cases = [("head", "repo", {"id": 11, "full_name": "other/project", "fork": False}),
                 ("head", "repo", {"id": 12, "full_name": REPO, "fork": False}),
                 ("head", "sha", OTHER), ("head", "ref", "new"), ("base", "ref", "new")]
        for side, field, value in cases:
            with self.subTest(side=side, field=field):
                self.fixture = Fixture()
                self.fixture.pr[side][field] = value
                self.assert_blocked()
        for field, value in (("observed_repository_id", 99), ("observed_pr_id", 99), ("observed_base_sha", OTHER)):
            self.fixture = Fixture()
            self.request = {**request(), field: value}
            self.assert_blocked()
        self.request = {**request(), "observed_repository_id": 11, "observed_pr_id": 13, "observed_base_sha": BASE}
        self.assertEqual(self.execute()["observation"], "ok")

    def test_same_repository_pr_within_a_fork_is_supported(self):
        for repo in (self.fixture.pr["head"]["repo"], self.fixture.pr["base"]["repo"],
                     self.fixture.run["repository"], self.fixture.run["head_repository"],
                     self.fixture.suite["repository"]):
            repo["fork"] = True
        self.assertEqual(self.execute()["observation"], "ok")

    def test_draft_closed_merged_conflicting_and_unknown_mergeability_are_reported(self):
        for change in ({"draft": True}, {"draft": False}, {"mergeable": False}, {"mergeable": None},
                       {"state": "closed"}, {"state": "closed", "merged": True}):
            with self.subTest(change=change):
                self.fixture = Fixture()
                self.fixture.pr.update(change)
                result = self.execute()
                self.assertEqual(result["observation"], "ok")
                for key, value in change.items():
                    self.assertEqual(result["pull_request"][key], value)

    def test_latest_run_beats_old_success_and_latest_attempt_never_uses_old_jobs(self):
        old = copy.deepcopy(self.fixture.run)
        old.update(id=20, run_number=4, run_attempt=8)
        self.fixture.runs = [old, self.fixture.run]
        for value in (self.fixture.run, self.fixture.suite, self.fixture.job, self.fixture.check):
            value["conclusion"] = "failure"
        result = self.execute()
        self.assertEqual(result["run"]["conclusion"], "failure")
        self.assertEqual(result["run"]["run_attempt"], 2)
        self.assertIn(JOBS_ENDPOINT, [c[-1] for c in self.fixture.calls])
        self.assertFalse(any("/attempts/8/" in c[-1] for c in self.fixture.calls))
        self.fixture = Fixture()
        self.fixture.job["run_attempt"] = 1
        self.assert_blocked("source_ambiguous")

    def test_greatest_run_number_not_list_order_or_prior_run_id_selects_latest(self):
        old = copy.deepcopy(self.fixture.run)
        old.update(id=100, run_number=4)
        self.fixture.runs = [old, self.fixture.run]
        self.assertEqual(self.execute()["run"]["id"], 21)

    def test_duplicate_job_check_run_producer_and_run_identities_are_ambiguous(self):
        self.fixture.jobs.append({**self.fixture.job, "id": 28})
        self.assert_blocked("source_ambiguous")
        self.fixture = Fixture()
        self.fixture.jobs.append(copy.deepcopy(self.fixture.job))
        self.assert_blocked("source_ambiguous")
        for change in ({"id": 22}, {"run_number": 6}, {}):
            self.fixture = Fixture()
            self.fixture.runs.append({**self.fixture.run, **change})
            self.assert_blocked("source_ambiguous")
        self.fixture = Fixture()
        self.request["required_jobs"] = ["test", "second"]
        self.fixture.jobs.append({**self.fixture.job, "id": 28, "name": "second"})
        self.assert_blocked("source_ambiguous")

    def test_untrusted_job_urls_are_never_followed(self):
        for url in ("https://attacker.invalid/check-runs/29", f"https://api.github.com/{PREFIX}/check-runs/29?x=1",
                    f"https://api.github.com/{PREFIX}/check-runs/../29", f"https://api.github.com/{PREFIX}/check-runs/29/",
                    f"https://api.github.com/{PREFIX}/check-runs/029", "https://api.github.com/repos/other/project/check-runs/29",
                    "file:///secret", "//api.github.com/check-runs/29"):
            with self.subTest(url=url):
                self.fixture = Fixture()
                self.fixture.job["check_run_url"] = url
                self.assert_blocked("source_ambiguous")
                self.assertFalse(any(c[-1] == CHECK_ENDPOINT for c in self.fixture.calls))

    def test_run_attempt_new_run_and_pr_drift_during_observation_discard_evidence(self):
        for target, changed, expected in (
                (RUN_ENDPOINT, {"run_attempt": 3}, "source_changed"),
                (RUN_ENDPOINT, {"status": "queued", "conclusion": None}, "source_changed"),
                (PR_ENDPOINT, {"head": {**self.fixture.pr["head"], "sha": OTHER}}, "head_changed"),
                (PR_ENDPOINT, {"base": {**self.fixture.pr["base"], "sha": OTHER}}, "base_changed"),
                (PR_ENDPOINT, {"base": {**self.fixture.pr["base"], "ref": "new"}}, "base_changed"),
                (PR_ENDPOINT, {"id": 14}, "source_ambiguous")):
            with self.subTest(target=target, changed=changed):
                self.fixture = Fixture()
                def hook(endpoint, count):
                    if endpoint == target and (target != PR_ENDPOINT or count == 2):
                        original = self.fixture.pr if target == PR_ENDPOINT else self.fixture.run
                        return response({**original, **changed})
                    return None
                self.fixture.hook = hook
                result = self.execute()
                self.assertEqual(result["error_code"], expected)
                self.assertEqual(result["observation"], "transient_error" if expected == "source_changed" else "blocked")
                self.assertFalse(result["complete"])
                self.assertIsNone(result["run"])
        self.fixture = Fixture()
        newer = {**self.fixture.run, "id": 22, "run_number": 6}
        self.fixture.hook = lambda endpoint, count: response({"total_count": 2, "workflow_runs": [self.fixture.run, newer]}) \
            if endpoint == LIST_ENDPOINT and count == 2 else None
        result = self.execute()
        self.assertEqual((result["observation"], result["error_code"]), ("transient_error", "source_changed"))

    def test_no_run_becoming_a_run_is_not_stable_pending_evidence(self):
        self.fixture.hook = lambda endpoint, count: response({"total_count": 0, "workflow_runs": []}) \
            if endpoint == LIST_ENDPOINT and count == 1 else None
        result = self.execute()
        self.assertEqual((result["observation"], result["error_code"]), ("transient_error", "source_changed"))

    def test_job_and_check_status_race_is_not_a_pass(self):
        self.fixture.check["conclusion"] = "failure"
        result = self.execute()
        self.assertEqual((result["observation"], result["error_code"]), ("transient_error", "source_changed"))

    def test_auth_permission_rate_and_server_errors_are_typed_without_raw_output(self):
        cases = [(401, {}, "blocked", "auth_required"), (403, {}, "blocked", "permission_denied"),
                 (404, {}, "blocked", "permission_denied"),
                 (403, {"X-RateLimit-Remaining": "0"}, "transient_error", "rate_limited"),
                 (403, {"Retry-After": "30"}, "transient_error", "rate_limited"),
                 (429, {}, "transient_error", "rate_limited"),
                 (503, {}, "transient_error", "service_unavailable"),
                 (302, {"Location": "https://attacker.invalid"}, "blocked", "protocol_invalid")]
        for status, headers, observation, code in cases:
            with self.subTest(status=status, headers=headers):
                self.fixture = Fixture()
                self.fixture.hook = lambda _, __: response({"message": "secret body"}, status, headers)
                result = self.execute()
                self.assertEqual(result["observation"], observation)
                self.assertEqual(result["error_code"], code)
                self.assertIsNone(result["detail"])
                self.assertNotIn("secret", json.dumps(result))
        for code, raw, expected in ((4, b"secret", "auth_required"), (1, b"secret", "source_unavailable"),
                                    (0, b"{}", "protocol_invalid")):
            self.fixture.hook = lambda _, __: (code, raw)
            self.assert_blocked(expected)

    def test_malformed_and_truncated_api_json_or_headers_do_not_emit_raw_evidence(self):
        for raw in (b"HTTP/2.0 200 OK\nContent-Type: application/json\n\n{",
                    b"HTTP/2.0 200 OK\nContent-Type: application/json\n\n{}{}",
                    b"HTTP/2.0 200 OK\nContent-Type: application/json\n\n[]",
                    b'HTTP/2.0 200 OK\nContent-Type: application/json\n\n{"id":1,"id":2}',
                    b"HTTP/2.0 200 OK\nContent-Type: text/html\n\nsecret",
                    b"HTTP/2.0 200 OK\nContent-Type: application/json\nContent-Type: text/html\n\n{}"):
            with self.subTest(raw=raw):
                self.fixture.hook = lambda _, __: (0, raw)
                self.assert_blocked("protocol_invalid")

    def test_deadline_request_and_total_byte_budgets_fail_closed(self):
        with patch.object(observer, "MAX_REQUESTS", 3):
            self.assert_blocked("request_limit")
        self.assertEqual(len(self.fixture.calls), 3)
        self.fixture = Fixture()
        with patch.object(observer, "MAX_TOTAL", 200):
            self.assert_blocked("response_limit")
        self.fixture = Fixture()
        with patch.object(observer.time, "monotonic", side_effect=[0, 31]):
            result = self.execute()
            self.assertEqual((result["observation"], result["error_code"]), ("transient_error", "request_timeout"))
        self.assertEqual(self.fixture.calls, [])

    def test_transport_refuses_all_non_whitelisted_endpoints_before_spawning(self):
        api = observer.GitHub(self.request, self.env)
        invalid = ["https://api.github.com/" + PR_ENDPOINT, "/" + PR_ENDPOINT,
                   PREFIX + "/actions/runs/21/rerun", PREFIX + "/pulls/7/merge",
                   PR_ENDPOINT + "?x=y", PR_ENDPOINT + "#secret", "repos/other/project/pulls/7",
                   LIST_ENDPOINT.replace("page=1", "page=4"), LIST_ENDPOINT.replace("pull_request", "push")]
        with patch.object(observer, "capture") as captured:
            for endpoint in invalid:
                with self.subTest(endpoint=endpoint), self.assertRaisesRegex(observer.ObservationError, "endpoint_invalid"):
                    api.get(endpoint)
        captured.assert_not_called()

    def test_capture_uses_no_shell_stdin_stderr_files_or_credential_logs(self):
        child = process(b"abc")
        with patch.object(subprocess, "Popen", return_value=child) as popen:
            self.assertEqual(observer.capture(["/host/bin/gh", "api"], {"GH_TOKEN": "secret"}), (0, b"abc"))
        self.assertEqual(popen.call_args.kwargs, {"stdin": subprocess.DEVNULL, "stdout": subprocess.PIPE,
                         "stderr": subprocess.DEVNULL, "env": {"GH_TOKEN": "secret"}, "shell": False})
        child = process(b"x" * (observer.MAX_RESPONSE + 1))
        with patch.object(subprocess, "Popen", return_value=child), \
                self.assertRaisesRegex(observer.ObservationError, "response_limit"):
            observer.capture(["/host/bin/gh", "api"], {})
        self.assertEqual(child.stdout.tell(), observer.MAX_RESPONSE + 1)
        child = process(b"x" * observer.MAX_RESPONSE)
        with patch.object(subprocess, "Popen", return_value=child):
            self.assertEqual(len(observer.capture(["/host/bin/gh", "api"], {})[1]), observer.MAX_RESPONSE)


class PaginationTests(unittest.TestCase):
    def fake(self, replies):
        api = MagicMock()
        api.get.side_effect = replies
        return api

    def test_complete_bounded_pages_and_exact_cap(self):
        api = self.fake([({"total_count": 201, "jobs": list(range(100))}, {}),
                         ({"total_count": 201, "jobs": list(range(100, 200))}, {}),
                         ({"total_count": 201, "jobs": [200]}, {})])
        self.assertEqual(observer.paginated(api, "fixed/jobs", "jobs"), list(range(201)))
        self.assertEqual([call.args[0] for call in api.get.call_args_list],
                         [f"fixed/jobs?per_page=100&page={n}" for n in (1, 2, 3)])
        api = self.fake([({"total_count": 300, "jobs": list(range(100))}, {})] * 3)
        self.assertEqual(len(observer.paginated(api, "fixed/jobs", "jobs")), 300)

    def test_truncated_unknown_oversized_and_contradictory_pagination_is_blocked(self):
        cases = [([({"total_count": 101, "jobs": []}, {})], "pagination_incomplete"),
                 ([({"total_count": 1, "jobs": []}, {})], "pagination_incomplete"),
                 ([({"total_count": 0, "jobs": [1]}, {})], "pagination_incomplete"),
                 ([({"total_count": 301, "jobs": list(range(100))}, {})], "pagination_incomplete"),
                 ([({"total_count": 1000, "jobs": list(range(100))}, {})], "pagination_incomplete"),
                 ([({"total_count": 1, "jobs": [1]}, {"link": b'<https://evil.invalid>; rel="next"'})], "pagination_incomplete"),
                 ([({"total_count": 101, "jobs": list(range(100))}, {}),
                   ({"total_count": 102, "jobs": [100, 101]}, {})], "source_changed"),
                 ([({"jobs": []}, {})], "protocol_invalid"),
                 ([({"total_count": True, "jobs": []}, {})], "protocol_invalid"),
                 ([({"total_count": 0, "jobs": {}}, {})], "protocol_invalid")]
        for replies, code in cases:
            with self.subTest(code=code, replies=str(replies)[:80]), self.assertRaisesRegex(observer.ObservationError, code):
                observer.paginated(self.fake(replies), "fixed/jobs", "jobs")


if __name__ == "__main__":
    unittest.main()
