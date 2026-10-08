#!/usr/bin/env python3
"""One bounded, read-only observation of host-pinned GitHub Actions evidence.

The host supplies one JSON object on stdin and RELAY_CI_OBSERVE=1 plus an
absolute RELAY_GH_PROGRAM. No subprocess is started without that opt-in. The
trusted Relay supervisor owns the deadline and entire process tree, including
blocked reads. This adapter never authenticates, changes GitHub, or polls.

Protocol v1 input: repository, pr_number, pr_url, head_sha, head_branch,
base_branch, workflow_id, app_id, event="pull_request", required_jobs,
observed_repository_id, observed_pr_id, observed_base_sha, version=1.
The three observed values are null initially and pinned by the host thereafter.
Output v1: observation, complete, repository, pull_request, run, error_code,
detail, and remote_merge_eligibility="not_established". Configured required
jobs are NOT evidence about GitHub branch protection or repository rulesets.

Sources: GitHub REST workflow-runs, workflow-jobs, checks/runs, checks/suites.
We bind workflow -> latest run/attempt -> jobs -> check-run ID/app/head/suite;
check names and an app ID alone do not identify a workflow producer.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


MAX_INPUT = 16 * 1024
MAX_OUTPUT = 64 * 1024
MAX_RESPONSE = 512 * 1024
MAX_TOTAL = 2 * 1024 * 1024
MAX_REQUESTS = 24
MAX_PAGES = 3
PAGE_SIZE = 100
MAX_SECONDS = 30
MAX_JOBS = 8
MAX_ID = 2**64 - 1
PENDING = {"queued", "in_progress", "waiting", "requested", "pending"}
CONCLUSIONS = {"success", "failure", "cancelled", "timed_out", "action_required",
               "neutral", "skipped", "stale", "startup_failure"}


class ObservationError(Exception):
    def __init__(self, code, transient=False):
        super().__init__(code)
        self.code = code
        self.transient = transient


def require(condition, code="protocol_invalid"):
    if not condition:
        raise ObservationError(code, transient=(code == "source_changed"))


def integer(value, allow_zero=False):
    require(type(value) is int and (0 if allow_zero else 1) <= value <= MAX_ID)
    return value


def string(value, maximum=256):
    require(isinstance(value, str) and 0 < len(value.encode("utf-8")) <= maximum)
    require(not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value))
    return value


def sha(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", value))
    return value


def branch(value):
    string(value)
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_./-]*", value) is not None)
    require(".." not in value and "//" not in value and not value.endswith("/"))
    require(not any(p.startswith(".") or p.endswith((".", ".lock")) for p in value.split("/")))
    return value


def repository_name(value):
    string(value, 201)
    require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value) is not None)
    require(not any(p in (".", "..") for p in value.split("/")))
    return value


def object_value(value):
    require(isinstance(value, dict))
    return value


def array(value):
    require(isinstance(value, list))
    return value


def boolean(value):
    require(type(value) is bool)
    return value


def no_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def decode_json(raw):
    try:
        return json.loads(raw.decode("utf-8", errors="strict"),
                          object_pairs_hook=no_duplicate_keys,
                          parse_constant=lambda _: require(False))
    except (ValueError, UnicodeError, RecursionError):
        raise ObservationError("protocol_invalid") from None


def parse_request(value):
    request = object_value(value)
    fields = {"version", "repository", "pr_number", "pr_url", "head_sha", "head_branch",
              "base_branch", "workflow_id", "app_id", "event", "required_jobs",
              "observed_repository_id", "observed_pr_id", "observed_base_sha"}
    require(set(request) == fields)
    require(type(request["version"]) is int and request["version"] == 1)
    repository = repository_name(request["repository"])
    integer(request["pr_number"])
    require(request["pr_url"] == f"https://github.com/{repository}/pull/{request['pr_number']}")
    sha(request["head_sha"])
    branch(request["head_branch"])
    branch(request["base_branch"])
    integer(request["workflow_id"])
    integer(request["app_id"])
    require(request["event"] == "pull_request")
    names = array(request["required_jobs"])
    require(1 <= len(names) <= MAX_JOBS)
    for name in names:
        string(name)
        require(name == name.strip())
    require(len(set(names)) == len(names), "source_ambiguous")
    for key in ("observed_repository_id", "observed_pr_id"):
        if request[key] is not None:
            integer(request[key])
    if request["observed_base_sha"] is not None:
        sha(request["observed_base_sha"])
    return request


def blank_result():
    return {"version": 1, "observation": "blocked", "complete": False,
            "repository": None, "pull_request": None, "run": None,
            "error_code": None, "detail": None,
            "remote_merge_eligibility": "not_established"}


def status(value):
    state = value.get("status")
    conclusion = value.get("conclusion")
    require(isinstance(state, str) and (state in PENDING or state == "completed"))
    if state == "completed":
        require(isinstance(conclusion, str) and conclusion in CONCLUSIONS)
    else:
        require(conclusion is None)
    return state, conclusion


def capture(command, env):
    """Bound retained bytes, suppress all untrusted stderr, inherit host lifetime.

    In particular GH_DEBUG cannot make credentials appear in Relay's logs.
    After overflow keep draining under the host deadline; never treat a prefix
    as evidence. No shell, stdin inheritance, pager, temporary file, or log.
    """
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, env=env, shell=False) as process:
        data = bytearray()
        count = 0
        while True:
            chunk = process.stdout.read(4096)
            if not chunk:
                break
            count += len(chunk)
            data.extend(chunk[:max(0, MAX_RESPONSE - len(data))])
        code = process.wait()
    require(count <= MAX_RESPONSE, "response_limit")
    return code, bytes(data)


class GitHub:
    def __init__(self, request, env):
        self.repository = request["repository"]
        self.program = env.get("RELAY_GH_PROGRAM", "")
        require(isinstance(self.program, str) and Path(self.program).is_absolute()
                and "\0" not in self.program, "configuration_invalid")
        self.env = dict(env)
        self.env.pop("GH_DEBUG", None)
        self.env.update({"GH_HOST": "github.com", "GH_PROMPT_DISABLED": "1", "GH_PAGER": ""})
        self.count = 0
        self.total = 0
        self.started = time.monotonic()

    def get(self, endpoint):
        # Check the final path even though every call site constructs it from
        # validated numeric identifiers. Never follow response-supplied URLs.
        prefix = f"repos/{self.repository}/"
        require(endpoint.startswith(prefix), "endpoint_invalid")
        suffix = endpoint[len(prefix):]
        n = r"[1-9][0-9]*"
        page = r"per_page=100&page=[1-3]"
        patterns = [rf"pulls/{n}", rf"check-suites/{n}", rf"check-runs/{n}",
                    rf"actions/runs/{n}", rf"actions/runs/{n}/attempts/{n}/jobs\?{page}",
                    rf"actions/workflows/{n}/runs\?head_sha=[0-9a-f]{{40}}(?:[0-9a-f]{{24}})?&event=pull_request&{page}"]
        require(any(re.fullmatch(p, suffix) for p in patterns), "endpoint_invalid")
        require(self.count < MAX_REQUESTS, "request_limit")
        if time.monotonic() - self.started > MAX_SECONDS:
            raise ObservationError("request_timeout", transient=True)
        self.count += 1
        command = [self.program, "api", "--hostname", "github.com", "--method", "GET",
                   "--include", "--header", "Accept: application/vnd.github+json",
                   "--header", "X-GitHub-Api-Version: 2022-11-28", endpoint]
        try:
            code, raw = capture(command, self.env)
        except OSError:
            raise ObservationError("observer_unavailable") from None
        self.total += len(raw)
        require(self.total <= MAX_TOTAL, "response_limit")
        if code == 4:
            raise ObservationError("auth_required")
        # gh --include writes the HTTP response headers before the JSON body.
        raw = raw.replace(b"\r\n", b"\n")
        headers, separator, body = raw.partition(b"\n\n")
        if not separator:
            raise ObservationError("source_unavailable" if code else "protocol_invalid")
        lines = headers.split(b"\n")
        match = re.fullmatch(rb"HTTP/[0-9.]+ ([0-9]{3})(?: [^\r\n]*)?", lines[0])
        require(match is not None)
        http_status = int(match[1])
        parsed = {}
        for line in lines[1:]:
            key, separator, value = line.partition(b":")
            require(separator and re.fullmatch(rb"[A-Za-z0-9-]+", key))
            key = key.decode("ascii").lower()
            require(key not in parsed)
            parsed[key] = value.strip()
        if http_status == 401:
            raise ObservationError("auth_required")
        if http_status == 429 or (http_status == 403 and
                (parsed.get("x-ratelimit-remaining") == b"0" or "retry-after" in parsed)):
            raise ObservationError("rate_limited", transient=True)
        if http_status in (403, 404):
            raise ObservationError("permission_denied")
        if http_status in (500, 502, 503, 504):
            raise ObservationError("service_unavailable", transient=True)
        require(http_status == 200 and code == 0)
        require(parsed.get("content-type", b"").split(b";", 1)[0]
                in (b"application/json", b"application/vnd.github+json"))
        return object_value(decode_json(body)), parsed


def paginated(api, endpoint, key):
    """Require complete pages and counts; never follow a Link target."""
    rows = []
    total = None
    for page in range(1, MAX_PAGES + 1):
        url = f"{endpoint}{'&' if '?' in endpoint else '?'}per_page={PAGE_SIZE}&page={page}"
        data, headers = api.get(url)
        count = integer(data.get("total_count"), allow_zero=True)
        require(count <= MAX_PAGES * PAGE_SIZE, "pagination_incomplete")
        require(total is None or total == count, "source_changed")
        total = count
        batch = array(data.get(key))
        require(len(batch) <= PAGE_SIZE and len(rows) + len(batch) <= total, "pagination_incomplete")
        rows.extend(batch)
        # Extra advertised pages contradict an apparently complete count.
        more = b'rel="next"' in headers.get("link", b"")
        if len(rows) == total:
            require(not more, "pagination_incomplete")
            return rows
        require(len(batch) == PAGE_SIZE, "pagination_incomplete")
    raise ObservationError("pagination_incomplete")


def verify_repository(value, request, expected_id=None):
    value = object_value(value)
    ident = integer(value.get("id"))
    require(value.get("full_name") == request["repository"], "source_ambiguous")
    if expected_id is not None:
        require(ident == expected_id, "source_ambiguous")
    # Numeric base/head identity excludes cross-repository PRs. The target
    # repository may itself be a fork; its ancestry is not an identity proof.
    return {"id": ident, "full_name": value["full_name"]}


def read_pr(api, request):
    value, _ = api.get(f"repos/{request['repository']}/pulls/{request['pr_number']}")
    ident = integer(value.get("id"))
    require(value.get("number") == request["pr_number"] and type(value.get("number")) is int,
            "source_ambiguous")
    require(value.get("html_url") == request["pr_url"], "source_ambiguous")
    if request["observed_pr_id"] is not None:
        require(ident == request["observed_pr_id"], "source_ambiguous")
    head, base = object_value(value.get("head")), object_value(value.get("base"))
    repo = verify_repository(base.get("repo"), request, request["observed_repository_id"])
    head_repo = verify_repository(head.get("repo"), request, repo["id"])
    require(value.get("state") in ("open", "closed"))
    merged, draft = boolean(value.get("merged")), boolean(value.get("draft"))
    require(not merged or value["state"] == "closed")
    mergeable = value.get("mergeable")
    require(mergeable is None or type(mergeable) is bool)
    result = {"id": ident, "number": request["pr_number"], "url": request["pr_url"],
              "state": value["state"], "merged": merged, "draft": draft,
              "head_sha": sha(head.get("sha")), "head_ref": branch(head.get("ref")),
              "head_repository_id": head_repo["id"], "base_ref": branch(base.get("ref")),
              "base_sha": sha(base.get("sha")), "base_repository_id": repo["id"],
              "mergeable": mergeable}
    return repo, result


def verify_pr_target(pr, request):
    require(pr["head_sha"] == request["head_sha"], "head_changed")
    require(pr["head_ref"] == request["head_branch"], "head_changed")
    require(pr["base_ref"] == request["base_branch"], "base_changed")
    if request["observed_base_sha"] is not None:
        require(pr["base_sha"] == request["observed_base_sha"], "base_changed")
    # Closed/merged/draft/mergeability are observations, not transport errors.
    # The host decides their lifecycle meaning and never equates configured
    # check success with remote merge eligibility.


def pr_links(value, request, pr, repo):
    links = array(value)
    require(1 <= len(links) <= 100, "source_ambiguous")
    ids = []
    for item in links:
        item = object_value(item)
        ident = integer(item.get("id"))
        require(ident not in ids, "source_ambiguous")
        ids.append(ident)
        # An Actions run simultaneously associated with another PR does not
        # satisfy this deliberately narrow exact-PR observation contract.
        require(ident == pr["id"] and item.get("number") == pr["number"], "source_ambiguous")
        for side, ref in (("head", request["head_branch"]), ("base", request["base_branch"])):
            link = object_value(item.get(side))
            require(link.get("ref") == ref, "source_ambiguous")
            require(integer(object_value(link.get("repo")).get("id")) == repo["id"], "source_ambiguous")
            if side == "head":
                require(link.get("sha") == request["head_sha"], "source_ambiguous")
            # A run's historical base SHA is not the current PR base SHA.
            sha(link.get("sha"))
    return ids


def normalize_run(value, request, pr, repo):
    value = object_value(value)
    state, conclusion = status(value)
    require(integer(value.get("workflow_id")) == request["workflow_id"], "source_ambiguous")
    require(value.get("event") == "pull_request", "source_ambiguous")
    require(value.get("head_sha") == request["head_sha"], "head_changed")
    require(value.get("head_branch") == request["head_branch"], "source_ambiguous")
    verify_repository(value.get("repository"), request, repo["id"])
    head = verify_repository(value.get("head_repository"), request, repo["id"])
    return {"id": integer(value.get("id")), "run_attempt": integer(value.get("run_attempt")),
            "run_number": integer(value.get("run_number")), "workflow_id": request["workflow_id"],
            "event": "pull_request", "head_sha": request["head_sha"],
            "head_repository_id": head["id"], "check_suite_id": integer(value.get("check_suite_id")),
            "app_id": request["app_id"], "status": state, "conclusion": conclusion,
            "pull_request_ids": pr_links(value.get("pull_requests"), request, pr, repo), "jobs": []}


def latest_run(api, request, pr, repo):
    endpoint = (f"repos/{request['repository']}/actions/workflows/{request['workflow_id']}/runs"
                f"?head_sha={request['head_sha']}&event=pull_request")
    runs = [normalize_run(item, request, pr, repo)
            for item in paginated(api, endpoint, "workflow_runs")]
    for key in ("id", "run_number"):
        require(len({run[key] for run in runs}) == len(runs), "source_ambiguous")
    return max(runs, key=lambda run: run["run_number"]) if runs else None


def verify_suite(api, request, pr, repo, run):
    value, _ = api.get(f"repos/{request['repository']}/check-suites/{run['check_suite_id']}")
    require(integer(value.get("id")) == run["check_suite_id"], "source_ambiguous")
    require(integer(object_value(value.get("app")).get("id")) == request["app_id"], "source_ambiguous")
    require(value.get("head_sha") == request["head_sha"], "head_changed")
    require(value.get("head_branch") == request["head_branch"], "source_ambiguous")
    verify_repository(value.get("repository"), request, repo["id"])
    pr_links(value.get("pull_requests"), request, pr, repo)
    status(value)


def read_jobs(api, request, run):
    endpoint = (f"repos/{request['repository']}/actions/runs/{run['id']}"
                f"/attempts/{run['run_attempt']}/jobs")
    values = paginated(api, endpoint, "jobs")
    selected, seen_ids, seen_checks = {}, set(), set()
    for value in values:
        value = object_value(value)
        job_id = integer(value.get("id"))
        require(job_id not in seen_ids, "source_ambiguous")
        seen_ids.add(job_id)
        require(integer(value.get("run_id")) == run["id"], "source_ambiguous")
        # Older API examples omit run_attempt; the exact attempt endpoint is
        # authoritative, and an explicit mismatching attempt is rejected.
        if "run_attempt" in value:
            require(integer(value["run_attempt"]) == run["run_attempt"], "source_ambiguous")
        require(value.get("head_sha") == request["head_sha"], "head_changed")
        name = string(value.get("name"))
        state, conclusion = status(value)
        if name not in request["required_jobs"]:
            continue
        require(name not in selected, "source_ambiguous")
        # Extract a numeric ID from an exact canonical URL; never request the
        # untrusted URL itself, only our reconstructed allowlisted endpoint.
        url = value.get("check_run_url")
        require(isinstance(url, str), "source_ambiguous")
        pattern = rf"https://api\.github\.com/repos/{re.escape(request['repository'])}/check-runs/([1-9][0-9]*)"
        match = re.fullmatch(pattern, url)
        require(match is not None, "source_ambiguous")
        check_id = integer(int(match[1]))
        require(check_id not in seen_checks, "source_ambiguous")
        seen_checks.add(check_id)
        check, _ = api.get(f"repos/{request['repository']}/check-runs/{check_id}")
        require(integer(check.get("id")) == check_id and check.get("name") == name, "source_ambiguous")
        require(check.get("head_sha") == request["head_sha"], "head_changed")
        require(integer(object_value(check.get("app")).get("id")) == request["app_id"], "source_ambiguous")
        require(integer(object_value(check.get("check_suite")).get("id")) == run["check_suite_id"], "source_ambiguous")
        require(status(check) == (state, conclusion), "source_changed")
        selected[name] = {"id": job_id, "check_run_id": check_id, "name": name,
                          "head_sha": request["head_sha"], "check_suite_id": run["check_suite_id"],
                          "app_id": request["app_id"], "status": state, "conclusion": conclusion}
    return [selected[name] for name in request["required_jobs"] if name in selected]


def observe(request, api):
    result = blank_result()
    try:
        repo, pr = read_pr(api, request)
        result.update(repository=repo, pull_request=pr)
        verify_pr_target(pr, request)
        run = latest_run(api, request, pr, repo)
        if run is not None:
            verify_suite(api, request, pr, repo, run)
            jobs = read_jobs(api, request, run)
            value, _ = api.get(f"repos/{request['repository']}/actions/runs/{run['id']}")
            require(normalize_run(value, request, pr, repo) == run, "source_changed")
        # A rerun/new run created while jobs were read must never leave an old
        # successful attempt labelled as the current one, including no-run.
        require(latest_run(api, request, pr, repo) == run, "source_changed")
        final_repo, final_pr = read_pr(api, request)
        require(final_repo == repo, "source_ambiguous")
        result["pull_request"] = final_pr
        require(final_pr["id"] == pr["id"], "source_ambiguous")
        verify_pr_target(final_pr, request)
        require(final_pr["base_sha"] == pr["base_sha"], "base_changed")
        require(final_pr == pr, "source_changed")
        if run is not None:
            run["jobs"] = jobs
        result.update(observation="ok", complete=True, run=run)
    except ObservationError as error:
        result.update(observation="transient_error" if error.transient else "blocked",
                      error_code=error.code)
    return result


def main(env=None, source=None):
    env = dict(os.environ if env is None else env)
    source = sys.stdin.buffer if source is None else source
    result = blank_result()
    try:
        require(env.get("RELAY_CI_OBSERVE") == "1", "observation_disabled")
        raw = source.read(MAX_INPUT + 1)
        require(isinstance(raw, bytes) and len(raw) <= MAX_INPUT, "input_limit")
        request = parse_request(decode_json(raw))
        result = observe(request, GitHub(request, env))
    except ObservationError as error:
        result.update(observation="transient_error" if error.transient else "blocked",
                      error_code=error.code)
    except (OSError, ValueError, UnicodeError, RecursionError):
        result["error_code"] = "protocol_invalid"
    output = json.dumps(result, separators=(",", ":"), ensure_ascii=True)
    if len(output.encode()) > MAX_OUTPUT:
        result = blank_result()
        result["error_code"] = "output_limit"
        output = json.dumps(result, separators=(",", ":"))
    print(output)
    # Typed failure is a completed read-only observation, not an ambiguous
    # external write. The host consumes the protocol rather than exit status.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
