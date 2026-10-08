#!/usr/bin/env python3
"""Bounded GitHub direct async-merge adapter; disabled unless the host opts in.

The host owns saved authorization, attempt fencing, durable write-intent and
receipt storage, process lifetime, and revocation. Each invocation does one
preflight, ready, merge, or read-only reconcile operation. It never retries a
write. Ready and merge additionally require RELAY_MERGE_WRITE=1. Existing gh
authentication is inherited, never installed, changed, or saved.

PUT merge-async (2026-03-10) is the ONLY merge write. SHA and non-bypass are
server-enforced; base, stack, and ready guards are preflight-only. Accepted
requests have no documented cancellation. A later deadline/revocation cannot
undo them, and read-only reconciliation remains necessary.

Primary contracts: https://docs.github.com/en/rest/pulls/pulls
and https://docs.github.com/en/graphql/reference/pulls . Offline fixtures only.
"""
import importlib.util
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import sys
import time


SPEC = importlib.util.spec_from_file_location(
    "relay_ci_source_checks", Path(__file__).with_name("github-ci-observe.py"))
ci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci)
Error = ci.ObservationError
require = ci.require
MAX_INPUT = 16 * 1024
MAX_OUTPUT = 64 * 1024
MAX_RESPONSE = ci.MAX_RESPONSE
MAX_TOTAL = 4 * 1024 * 1024
MAX_REQUESTS = 32
MAX_SECONDS = 30
MAX_GATE = 4096
API_VERSION = "2026-03-10"
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
OPERATIONS = {"preflight", "ready", "merge", "reconcile"}
# Fixed queries only. No supplied GraphQL document, alias, field, or URL is run.
# Both nullable stack fields must be explicitly present and null. The API can
# merge a whole stack, so even a one-entry stack is outside this adapter's scope.
PREFLIGHT_QUERY = """query RelayMergePreflight($owner:String!,$name:String!,$number:Int!){
  repository(owner:$owner,name:$name){databaseId nameWithOwner
    pullRequest(number:$number){id fullDatabaseId number url state merged isDraft
      headRefName headRefOid baseRefName baseRefOid
      headRepository{databaseId nameWithOwner}
      baseRepository{databaseId nameWithOwner}
      baseRef{name target{oid}}
      mergeable mergeStateStatus
      stack{id} stackEntry{id}
      isInMergeQueue isMergeQueueEnabled mergeQueue{id} mergeQueueEntry{id}
      autoMergeRequest{enabledAt}
    }
  }
}"""
READY_MUTATION = """mutation RelayReady($id:ID!){
  markPullRequestReadyForReview(input:{pullRequestId:$id}){
    pullRequest{id fullDatabaseId number isDraft}
  }
}"""


def node_id(value):
    ci.string(value, 256)
    require(re.fullmatch(r"[A-Za-z0-9_=-]+", value) is not None)
    return value


def uuid(value):
    require(isinstance(value, str) and re.fullmatch(UUID, value) is not None)
    return value


def options(request):
    return {"sha": request["head_sha"], "merge_method": request["merge_method"],
            "merge_action": "direct_merge", "bypass_rules": False}


def source_request(request):
    return {"version": 1, "repository": request["repository"],
            "pr_number": request["pr_number"], "pr_url": request["pr_url"],
            "head_sha": request["head_sha"], "head_branch": request["head_branch"],
            "base_branch": request["base_branch"], **request["ci_source"],
            "observed_repository_id": request["repository_id"],
            "observed_pr_id": request["pr_id"], "observed_base_sha": None}


def parse_request(value):
    request = ci.object_value(value)
    fields = {"version", "operation", "authorization_id", "attempt", "repository",
              "repository_id", "pr_number", "pr_id", "pr_node_id", "pr_url",
              "head_sha", "head_branch", "base_branch", "ci_source", "merge_method",
              "target_guard", "allow_ready", "accept_non_atomic_target_guard",
              "deadline_semantics", "deadline", "async_request"}
    require(set(request) == fields)
    require(type(request["version"]) is int and request["version"] == 1)
    require(isinstance(request["operation"], str) and request["operation"] in OPERATIONS)
    for key in ("authorization_id", "attempt", "repository_id", "pr_id", "deadline"):
        ci.integer(request[key])
    if request["pr_node_id"] is not None:
        node_id(request["pr_node_id"])
    if request["operation"] in {"ready", "merge"}:
        require(request["pr_node_id"] is not None, "node_identity_unpinned")
    require(set(ci.object_value(request["ci_source"])) ==
            {"workflow_id", "app_id", "event", "required_jobs"})
    ci.parse_request(source_request(request))
    require(request["merge_method"] in ("merge", "squash", "rebase"))
    require(request["target_guard"] == "preflight_only")
    require(request["accept_non_atomic_target_guard"] is True)
    require(request["deadline_semantics"] == "last_dispatch")
    ci.boolean(request["allow_ready"])
    record = request["async_request"]
    if record is not None:
        require(set(ci.object_value(record)) == {"id", "options", "provenance"})
        uuid(record["id"])
        require(ci.object_value(record["options"]) == options(request))
        require(record["options"]["bypass_rules"] is False)
        require(record["provenance"] in ("relay", "external_unknown"))
        require(request["operation"] == "reconcile", "write_already_recorded")
    if request["operation"] == "ready":
        require(request["allow_ready"], "ready_not_authorized")
    return request


def blank_result(operation=None):
    return {"version": 1, "operation": operation, "status": "blocked", "complete": False,
            "effect": "none", "ci_observation": None, "target": None,
            "async_request": None, "merge_commit_sha": None,
            "error_code": None, "detail": None}


# Kept separately patchable: tests prohibit every real subprocess.
capture = ci.capture


def gate_metadata(value, allow_zero=False):
    require(isinstance(value, str) and re.fullmatch(r"0|[1-9][0-9]{0,19}", value) is not None,
            "write_gate_invalid")
    number = int(value)
    require((0 if allow_zero else 1) <= number <= ci.MAX_ID, "write_gate_invalid")
    return number


def verify_gate_file(fd, path, device, inode):
    opened = os.fstat(fd)
    current = os.stat(path, follow_symlinks=False)
    for value in (opened, current):
        require(stat.S_ISREG(value.st_mode) and stat.S_IMODE(value.st_mode) == 0o600 and
                value.st_uid == os.geteuid() and value.st_nlink == 1 and
                value.st_dev == device and value.st_ino == inode, "write_gate_invalid")


@contextlib.contextmanager
def write_gate(api):
    """Serialize dispatch against host revocation without locking preflight reads.

    The host's persistent attempt ownership admits one adapter for this phase.
    Its revoker takes LOCK_EX and therefore either commits revocation before
    this LOCK_SH, or waits until this already-dispatched capture finishes.
    No gate is manufactured here and no missing/invalid gate has a fallback.
    """
    env, request = api.env, api.request
    path = env.get("RELAY_MERGE_GATE_PATH", "")
    binding = env.get("RELAY_MERGE_GATE_BINDING", "")
    require(isinstance(path, str) and Path(path).is_absolute() and "\0" not in path
            and ".." not in Path(path).parts, "write_gate_invalid")
    require(isinstance(binding, str) and re.fullmatch(r"[0-9a-f]{64}", binding) is not None,
            "write_gate_invalid")
    device = gate_metadata(env.get("RELAY_MERGE_GATE_DEVICE"), allow_zero=True)
    inode = gate_metadata(env.get("RELAY_MERGE_GATE_INODE"))
    fd = None
    try:
        fd = os.open(path, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
        verify_gate_file(fd, path, device, inode)
        while True:
            require(time.time() < request["deadline"], "authorization_expired")
            require(time.monotonic() - api.started < MAX_SECONDS, "write_gate_timeout")
            try:
                fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                time.sleep(0.02)
        verify_gate_file(fd, path, device, inode)
        raw = os.read(fd, MAX_GATE + 1)
        require(len(raw) <= MAX_GATE, "write_gate_invalid")
        try:
            value = ci.object_value(ci.decode_json(raw))
        except Error:
            raise Error("write_gate_invalid") from None
        require(set(value) == {"version", "authorization_id", "attempt", "phase", "consent_sha256",
                              "deadline", "revoked", "write_started"}, "write_gate_invalid")
        expected = {"version": 1, "authorization_id": request["authorization_id"],
                    "attempt": request["attempt"], "phase": request["operation"],
                    "consent_sha256": binding, "deadline": request["deadline"]}
        require(all(value[key] == expected_value for key, expected_value in expected.items()) and
                all(type(value[key]) is int for key in ("version", "authorization_id", "attempt", "deadline")) and
                type(value["revoked"]) is bool and type(value["write_started"]) is bool,
                "write_gate_invalid")
        require(not value["revoked"], "authorization_revoked")
        require(not value["write_started"], "write_already_attempted")
        require(time.time() < request["deadline"], "authorization_expired")
        value["write_started"] = True
        encoded = json.dumps(value, separators=(",", ":")).encode("ascii")
        os.lseek(fd, 0, os.SEEK_SET)
        remaining = memoryview(encoded)
        while remaining:
            written = os.write(fd, remaining)
            require(written > 0, "write_gate_invalid")
            remaining = remaining[written:]
        os.ftruncate(fd, len(encoded))
        os.fsync(fd)
        verify_gate_file(fd, path, device, inode)
        require(time.time() < request["deadline"], "authorization_expired")
        yield
    except OSError:
        raise Error("write_gate_invalid") from None
    finally:
        if fd is not None:
            os.close(fd)  # Releases the shared lock on success, failure, or loss.


class GitHub:
    def __init__(self, request, env):
        require(env.get("RELAY_MERGE_CONTROL") == "1", "merge_control_disabled")
        if request["operation"] in {"ready", "merge"}:
            require(env.get("RELAY_MERGE_WRITE") == "1", "merge_write_disabled")
        self.request = request
        self.repository = request["repository"]
        self.program = env.get("RELAY_GH_PROGRAM", "")
        require(isinstance(self.program, str) and Path(self.program).is_absolute()
                and "\0" not in self.program, "configuration_invalid")
        self.env = dict(env)
        self.env.pop("GH_DEBUG", None)
        self.env.update({"GH_HOST": "github.com", "GH_PROMPT_DISABLED": "1", "GH_PAGER": ""})
        self.count = self.total = 0
        self.started = time.monotonic()
        self.effect = "none"
        self.pr_node_id = request["pr_node_id"]

    def validate_route(self, method, endpoint, fields):
        request = self.request
        prefix = f"repos/{self.repository}"
        pull = f"{prefix}/pulls/{request['pr_number']}"
        if method == "GET":
            require(fields is None, "endpoint_invalid")
            n = r"[1-9][0-9]*"
            page = r"per_page=100&page=[1-3]"
            suffix = endpoint[len(prefix) + 1:] if endpoint.startswith(prefix + "/") else None
            patterns = [rf"check-suites/{n}", rf"check-runs/{n}", rf"actions/runs/{n}",
                        rf"actions/runs/{n}/attempts/{n}/jobs\?{page}",
                        rf"actions/workflows/{request['ci_source']['workflow_id']}/runs\?"
                        rf"head_sha={request['head_sha']}&event=pull_request&{page}"]
            allowed = endpoint in (prefix, pull) or (suffix is not None and
                      any(re.fullmatch(p, suffix) for p in patterns))
            record = request["async_request"]
            allowed |= record is not None and endpoint == f"{pull}/merge-async/{record['id']}"
            require(allowed, "endpoint_invalid")
            return False
        if method == "POST" and endpoint == "graphql":
            owner, name = self.repository.split("/")
            if fields == {"query": PREFLIGHT_QUERY, "owner": owner, "name": name,
                          "number": request["pr_number"]}:
                return False
            require(request["operation"] == "ready" and request["allow_ready"] and
                    request["pr_node_id"] is not None and
                    fields == {"query": READY_MUTATION, "id": request["pr_node_id"]}, "endpoint_invalid")
            return True
        require(method == "PUT" and endpoint == f"{pull}/merge-async" and
                request["operation"] == "merge" and fields == options(request) and
                fields["bypass_rules"] is False, "endpoint_invalid")
        return True

    def request_json(self, method, endpoint, fields=None):
        mutation = self.validate_route(method, endpoint, fields)
        require(self.count < MAX_REQUESTS, "request_limit")
        if time.monotonic() - self.started >= MAX_SECONDS:
            raise Error("request_timeout", transient=True)
        if mutation:
            require(self.env.get("RELAY_MERGE_WRITE") == "1", "merge_write_disabled")
            require(time.time() < self.request["deadline"], "authorization_expired")
            require(self.effect == "none", "write_already_attempted")
        self.count += 1
        command = [self.program, "api", "--hostname", "github.com", "--method", method,
                   "--include", "--header", "Accept: application/vnd.github+json",
                   "--header", f"X-GitHub-Api-Version: {API_VERSION}"]
        for key, value in (fields or {}).items():
            # Raw strings cannot expand @files, and typed scalars cannot inject
            # arguments. No shell, URL interpolation, credentials, or input file.
            if isinstance(value, str):
                command += ["--raw-field", f"{key}={value}"]
            else:
                command += ["--field", f"{key}={json.dumps(value)}"]
        command.append(endpoint)
        try:
            if mutation:
                with write_gate(self):
                    self.effect = "unknown"  # Gate's durable marker precedes dispatch.
                    code, raw = capture(command, self.env)
            else:
                code, raw = capture(command, self.env)
        except OSError:
            raise Error("adapter_unavailable") from None
        require(isinstance(raw, bytes) and len(raw) <= MAX_RESPONSE, "response_limit")
        self.total += len(raw)
        require(self.total <= MAX_TOTAL, "response_limit")
        if code == 4:
            raise Error("auth_required")
        headers, separator, body = raw.replace(b"\r\n", b"\n").partition(b"\n\n")
        require(separator, "response_invalid")
        lines = headers.split(b"\n")
        match = re.fullmatch(rb"HTTP/[0-9.]+ ([0-9]{3})(?: [^\r\n]*)?", lines[0])
        require(match is not None, "response_invalid")
        http_status = int(match[1])
        parsed = {}
        for line in lines[1:]:
            key, separator, value = line.partition(b":")
            require(separator and re.fullmatch(rb"[A-Za-z0-9-]+", key), "response_invalid")
            key = key.decode("ascii").lower()
            require(key not in parsed, "response_invalid")
            parsed[key] = value.strip()
        if mutation and http_status in (400, 401, 403, 404, 422, 429):
            self.effect = "none"  # Explicit rejection, with no accepted request.
        if http_status == 401:
            raise Error("auth_required")
        if http_status == 429 or (http_status == 403 and
                (parsed.get("x-ratelimit-remaining") == b"0" or "retry-after" in parsed)):
            raise Error("rate_limited", transient=True)
        if http_status == 403:
            raise Error("permission_denied")
        if http_status == 404:
            if "/merge-async/" in endpoint:
                raise Error("result_expired_or_unavailable")
            raise Error("async_api_unavailable" if mutation else "permission_denied")
        if http_status in (400, 422) and mutation:
            raise Error("github_merge_rejected" if method == "PUT" else "github_ready_rejected")
        if http_status in (500, 502, 503, 504):
            raise Error("service_unavailable", transient=True)
        allowed = (200, 202, 409) if method == "PUT" else (200,)
        require(http_status in allowed, "response_invalid")
        require(code == 0 or (method == "PUT" and http_status == 409 and code == 1), "response_invalid")
        require(parsed.get("content-type", b"").split(b";", 1)[0]
                in (b"application/json", b"application/vnd.github+json"), "response_invalid")
        return http_status, ci.object_value(ci.decode_json(body)), parsed

    def get(self, endpoint):
        _, value, headers = self.request_json("GET", endpoint)
        if endpoint == f"repos/{self.repository}/pulls/{self.request['pr_number']}":
            remote_node = node_id(value.get("node_id"))
            require(self.pr_node_id is None or remote_node == self.pr_node_id, "node_identity_changed")
            self.pr_node_id = remote_node
        return value, headers

    def graphql(self):
        owner, name = self.repository.split("/")
        _, value, _ = self.request_json("POST", "graphql", {
            "query": PREFLIGHT_QUERY, "owner": owner, "name": name,
            "number": self.request["pr_number"]})
        require("errors" not in value, "graphql_read_incomplete")
        return ci.object_value(ci.object_value(value.get("data")).get("repository"))


def numeric_graphql_id(value):
    # GitHub's BigInt scalar serializes large PR IDs as decimal strings.
    if isinstance(value, str):
        require(re.fullmatch(r"[1-9][0-9]{0,19}", value) is not None)
        value = int(value)
    return ci.integer(value)


def read_target(api, request, repository):
    value = api.graphql()
    require(value.get("databaseId") == request["repository_id"] and
            type(value.get("databaseId")) is int and
            value.get("nameWithOwner") == request["repository"], "source_ambiguous")
    pr = ci.object_value(value.get("pullRequest"))
    require(numeric_graphql_id(pr.get("fullDatabaseId")) == request["pr_id"] and
            pr.get("number") == request["pr_number"] and type(pr.get("number")) is int and
            pr.get("url") == request["pr_url"], "source_ambiguous")
    require(node_id(pr.get("id")) == api.pr_node_id, "node_identity_changed")
    for key in ("headRepository", "baseRepository"):
        repo = ci.object_value(pr.get(key))
        require(repo.get("databaseId") == request["repository_id"] and
                type(repo.get("databaseId")) is int and
                repo.get("nameWithOwner") == request["repository"], "source_ambiguous")
    require(pr.get("headRefOid") == request["head_sha"] and
            pr.get("headRefName") == request["head_branch"], "head_changed")
    require(pr.get("baseRefName") == request["base_branch"], "base_changed")
    base_sha = ci.sha(pr.get("baseRefOid"))
    base_ref = ci.object_value(pr.get("baseRef"))
    require(base_ref.get("name") == request["base_branch"] and
            ci.object_value(base_ref.get("target")).get("oid") == base_sha, "base_changed")
    require(pr.get("state") in ("OPEN", "CLOSED", "MERGED"))
    draft, merged = ci.boolean(pr.get("isDraft")), ci.boolean(pr.get("merged"))
    require(merged == (pr["state"] == "MERGED"))
    require(pr.get("mergeable") in ("MERGEABLE", "CONFLICTING", "UNKNOWN"))
    require(pr.get("mergeStateStatus") in
            ("BEHIND", "BLOCKED", "CLEAN", "DIRTY", "DRAFT", "HAS_HOOKS", "UNKNOWN", "UNSTABLE"))
    for key, code in (("stack", "stack_unsupported"), ("stackEntry", "stack_unsupported"),
                      ("mergeQueue", "queue_unsupported"), ("mergeQueueEntry", "queue_unsupported"),
                      ("autoMergeRequest", "auto_merge_unsupported")):
        require(key in pr, "graphql_read_incomplete")
        require(pr[key] is None, code)
    for key in ("isInMergeQueue", "isMergeQueueEnabled"):
        require(not ci.boolean(pr.get(key)), "queue_unsupported")
    target = {"repository_id": request["repository_id"], "pr_id": request["pr_id"],
              "pr_node_id": api.pr_node_id, "head_sha": request["head_sha"],
              "head_branch": request["head_branch"], "base_branch": request["base_branch"],
              "base_sha": base_sha, "draft": draft,
              "state": "open" if pr["state"] == "OPEN" else "closed", "merged": merged,
              "stack_clear": True, "queue_clear": True, "auto_merge_disabled": True,
              "delete_branch_on_merge": repository.get("delete_branch_on_merge")}
    return target, pr["mergeable"], pr["mergeStateStatus"]


def checks_passed(observation, request):
    run = observation["run"]
    return (run is not None and run["status"] == "completed" and run["conclusion"] == "success" and
            len(run["jobs"]) == len(request["ci_source"]["required_jobs"]) and
            all(job["status"] == "completed" and job["conclusion"] == "success" for job in run["jobs"]))


def preflight(request, api, result):
    source = source_request(request)
    repository, _ = api.get(f"repos/{request['repository']}")
    ci.verify_repository(repository, source, request["repository_id"])
    if "delete_branch_on_merge" in repository:
        ci.boolean(repository["delete_branch_on_merge"])
    method_key = {"merge": "allow_merge_commit", "squash": "allow_squash_merge",
                  "rebase": "allow_rebase_merge"}[request["merge_method"]]
    require(ci.boolean(repository.get(method_key)), "merge_method_unavailable")
    # Resolve GraphQL's opaque node address only from the fixed numeric REST PR.
    _, first_pr = ci.read_pr(api, source)
    ci.verify_pr_target(first_pr, source)
    first = read_target(api, request, repository)
    require(first[0]["base_sha"] == first_pr["base_sha"], "base_changed")
    require(first[0]["draft"] == first_pr["draft"] and first[0]["state"] == first_pr["state"] and
            first[0]["merged"] == first_pr["merged"], "source_changed")
    result["target"] = first[0]
    if first[0]["merged"]:
        result.update(status="externally_merged", complete=True, effect="externally_merged")
        return False
    require(first[0]["state"] == "open", "pr_closed")
    require(first[1] != "CONFLICTING" and first[2] != "DIRTY", "merge_conflict")
    require(first[1] != "UNKNOWN" and first[2] != "UNKNOWN", "remote_state_unknown")
    observation = ci.observe(source, api)
    result["ci_observation"] = observation
    if not observation["complete"]:
        raise Error(observation["error_code"] or "ci_observation_incomplete",
                    transient=observation["observation"] == "transient_error")
    observed_pr = observation["pull_request"]
    require(observed_pr["base_sha"] == first[0]["base_sha"], "base_changed")
    require(observed_pr["draft"] == first[0]["draft"] and
            observed_pr["state"] == first[0]["state"] and not observed_pr["merged"], "source_changed")
    final = read_target(api, request, repository)
    require(final[0]["base_sha"] == first[0]["base_sha"], "base_changed")
    require(final == first, "source_changed")
    result["target"] = final[0]
    green = checks_passed(observation, request)
    # Ready is a distinct authorized effect and can trigger draft-gated CI.
    # For merge, configured checks + remote state are both required. GitHub's
    # explicit bypass_rules=false remains the final protected-rule authority.
    if green and request["operation"] != "ready" and not final[0]["draft"]:
        require(final[1] == "MERGEABLE" and final[2] in ("CLEAN", "HAS_HOOKS"),
                "remote_merge_blocked")
    result.update(status="preflight_ready" if green else "waiting_ci", complete=True)
    return green


def pending_record(value, request, provenance, expected_id=None):
    detail = ci.object_value(value.get("details"))
    require({"uuid", "expected_head_sha", "merge_method", "merge_action", "bypass_rules"}
            <= set(detail), "async_response_unknown")
    ident = uuid(detail.get("uuid"))
    ci.sha(detail["expected_head_sha"])
    require(detail["merge_method"] in ("merge", "squash", "rebase"), "async_response_unknown")
    require(detail["merge_action"] in ("default", "direct_merge", "merge_queue"), "async_response_unknown")
    ci.boolean(detail["bypass_rules"])
    require(expected_id is None or ident == expected_id, "async_request_mismatch")
    require(detail.get("expected_head_sha") == request["head_sha"] and
            detail.get("merge_method") == request["merge_method"] and
            detail.get("merge_action") == "direct_merge" and
            detail.get("bypass_rules") is False, "async_request_mismatch")
    return {"id": ident, "options": options(request), "provenance": provenance}


def handle_async(value, request, result, http_status, record=None):
    state = value.get("status")
    detail = ci.object_value(value.get("details"))
    if record is None and http_status in (202, 409):
        require(state == "pending", "async_response_unknown")
        accepted = pending_record(value, request, "relay" if http_status == 202 else "external_unknown")
        result.update(status="accepted" if http_status == 202 else "pending", complete=True,
                      effect="merge_request_recorded" if http_status == 202 else "none", async_request=accepted)
        return
    if state == "pending" and record is not None:
        accepted = pending_record(value, request, record["provenance"], record["id"])
        result.update(status="pending", complete=True, effect="none", async_request=accepted)
    elif state == "merged":
        merged_sha = ci.sha(detail.get("sha"))
        owned = record is not None and record["provenance"] == "relay"
        result.update(status="merged" if owned else "externally_merged", complete=True,
                      effect="merge_confirmed" if owned else "externally_merged", merge_commit_sha=merged_sha)
    elif state == "enqueued":
        result.update(status="enqueued", complete=True, effect="none", error_code="queue_unsupported")
    elif state == "failed" and record is not None:
        ci.string(detail.get("message"), 4096)
        result.update(status="failed", complete=True, effect="none", error_code="github_async_merge_failed")
    else:
        raise Error("async_response_unknown")


def reconcile(request, api, result):
    record = request["async_request"]
    result["async_request"] = record
    if record is not None:
        endpoint = f"repos/{request['repository']}/pulls/{request['pr_number']}/merge-async/{record['id']}"
        _, value, _ = api.request_json("GET", endpoint)
        handle_async(value, request, result, 200, record)
        return
    source = source_request(request)
    _, pr = ci.read_pr(api, source)
    ci.verify_pr_target(pr, source)
    if pr["merged"]:
        result.update(status="externally_merged", complete=True, effect="externally_merged")
    else:
        result.update(status="effect_unknown", effect="unknown", error_code="effect_unresolved")


def execute(request, api):
    result = blank_result(request["operation"])
    try:
        if request["operation"] == "reconcile":
            reconcile(request, api, result)
            return result
        require(time.time() < request["deadline"], "authorization_expired")
        green = preflight(request, api, result)
        if result["status"] == "externally_merged" or request["operation"] == "preflight":
            return result
        if request["operation"] == "ready":
            require(result["target"]["draft"], "already_ready_requires_observation")
            _, value, _ = api.request_json("POST", "graphql", {
                "query": READY_MUTATION, "id": request["pr_node_id"]})
            require("errors" not in value, "ready_response_unknown")
            data = ci.object_value(ci.object_value(value.get("data")).get("markPullRequestReadyForReview"))
            pr = ci.object_value(data.get("pullRequest"))
            require(pr.get("id") == request["pr_node_id"] and
                    numeric_graphql_id(pr.get("fullDatabaseId")) == request["pr_id"] and
                    pr.get("number") == request["pr_number"] and type(pr.get("number")) is int and
                    pr.get("isDraft") is False, "ready_response_unknown")
            result.update(status="ready_confirmed", complete=True, effect="ready_confirmed")
        else:
            require(not result["target"]["draft"], "draft_requires_ready")
            if not green:
                return result
            status, value, _ = api.request_json("PUT",
                f"repos/{request['repository']}/pulls/{request['pr_number']}/merge-async", options(request))
            try:
                handle_async(value, request, result, status)
            except Error as error:
                # A well-formed collision with different options is an explicit
                # refusal to adopt another request. Missing UUID or malformed
                # output remains unknown, including a malformed 409 envelope.
                if status == 409 and error.code == "async_request_mismatch":
                    api.effect = "none"
                raise
    except Error as error:
        unknown = api.effect == "unknown" or request["operation"] == "reconcile"
        result.update(status="effect_unknown" if unknown else
                      ("transient_error" if error.transient else "blocked"), complete=False,
                      effect="unknown" if unknown else "none", error_code=error.code)
    return result


def main(env=None, source=None):
    env = dict(os.environ if env is None else env)
    source = sys.stdin.buffer if source is None else source
    result = blank_result()
    api = None
    try:
        require(env.get("RELAY_MERGE_CONTROL") == "1", "merge_control_disabled")
        raw = source.read(MAX_INPUT + 1)
        require(isinstance(raw, bytes) and len(raw) <= MAX_INPUT, "input_limit")
        request = parse_request(ci.decode_json(raw))
        result["operation"] = request["operation"]
        api = GitHub(request, env)
        result = execute(request, api)
    except Error as error:
        result["error_code"] = error.code
    except (OSError, ValueError, UnicodeError, RecursionError, TypeError, KeyError):
        result["error_code"] = "protocol_invalid"
        if api is not None and api.effect == "unknown":
            result.update(status="effect_unknown", effect="unknown", complete=False)
    output = json.dumps(result, separators=(",", ":"), ensure_ascii=True)
    if len(output.encode()) > MAX_OUTPUT:
        fallback = blank_result(result["operation"])
        fallback.update(error_code="output_limit", status="effect_unknown" if api and api.effect == "unknown" else "blocked",
                        effect="unknown" if api and api.effect == "unknown" else "none")
        output = json.dumps(fallback, separators=(",", ":"))
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
