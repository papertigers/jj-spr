"""A fake GitHub API for testing jj-spr's native stack support.

It serves the REST and GraphQL calls jj-spr makes, backed by a bare Git
repository that jj-spr pushes to, and models GitHub's stacked pull requests.

The stack behaviour follows the fake GitHub in jj-stack
(https://github.com/bos/jj-stack, tests/support/fake_github.py, Apache-2.0),
whose authors checked it against the real API:

- a stack needs two or more pull requests, each PR's base must be the head
  branch of the PR below it, and a PR can be in only one stack
- the base of a stacked PR cannot be changed with PATCH (422)
- stacked PRs are merged with PUT .../merge-async and polled with
  GET .../merge-async/{uuid}; merging a PR merges the stack prefix below it,
  and GitHub then rebases the remaining members onto the new base
- merged members stay listed in the stack as history

Behaviour that is an assumption of this fake rather than observed:

- the ordinary merge endpoint refuses stacked PRs
- the post-merge rebase replays each surviving PR's net diff as one commit

Everything else (closing PRs when their base or head branch is deleted,
squash merges as a three-way merge) is ordinary GitHub behaviour.

Only the Python standard library is used, so this runs anywhere jj-spr does.
"""

from __future__ import annotations

import json
import re
import subprocess
import threading
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

_GIT_ENV = {
    "GIT_AUTHOR_NAME": "Fake GitHub",
    "GIT_AUTHOR_EMAIL": "fake-github@example.com",
    "GIT_COMMITTER_NAME": "Fake GitHub",
    "GIT_COMMITTER_EMAIL": "fake-github@example.com",
}


class ApiError(Exception):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


def _now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


@dataclass
class PullRequest:
    number: int
    title: str
    body: str
    head_ref: str
    base_ref: str
    draft: bool
    state: str = "open"
    merged_at: str | None = None
    merge_commit_sha: str | None = None
    last_head_sha: str = ""
    closed_reason: str | None = None


@dataclass
class MergeOperation:
    uuid: str
    pr_number: int
    method: str
    expected_head: str
    status: str = "pending"
    message: str = "Merge request enqueued."
    sha: str | None = None


@dataclass
class FakeGitHub:
    """State of one fake repository."""

    owner: str
    name: str
    git_dir: Path
    stacks_enabled: bool = True
    prs: dict[int, PullRequest] = field(default_factory=dict)
    stacks: dict[int, list[int]] = field(default_factory=dict)
    merge_ops: dict[int, MergeOperation] = field(default_factory=dict)
    requests: list[tuple[str, str, object]] = field(default_factory=list)
    next_number: int = 1
    next_stack: int = 1
    lock: threading.RLock = field(default_factory=threading.RLock)

    # -- git -------------------------------------------------------------

    def git(self, *args: str, stdin: str | None = None) -> str:
        done = subprocess.run(
            ["git", "--git-dir", str(self.git_dir), *args],
            capture_output=True,
            text=True,
            input=stdin,
            env={**_base_env(), **_GIT_ENV},
        )
        if done.returncode != 0:
            raise AssertionError(f"fake github: git {args} failed: {done.stderr}")
        return done.stdout.strip()

    def branches(self) -> dict[str, str]:
        done = subprocess.run(
            ["git", "--git-dir", str(self.git_dir), "show-ref", "--heads"],
            capture_output=True,
            text=True,
        )
        heads = {}
        for line in done.stdout.splitlines():
            sha, _, ref = line.partition(" ")
            heads[ref.removeprefix("refs/heads/")] = sha
        return heads

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        return (
            subprocess.run(
                ["git", "--git-dir", str(self.git_dir), "merge-base", "--is-ancestor",
                 ancestor, descendant],
                capture_output=True,
            ).returncode
            == 0
        )

    # -- pull request state ------------------------------------------------

    def refresh(self) -> None:
        """Apply what GitHub does on its own when branches change."""
        heads = self.branches()
        for pr in self.prs.values():
            if pr.state != "open":
                continue
            if pr.head_ref in heads:
                pr.last_head_sha = heads[pr.head_ref]
            if pr.base_ref not in heads:
                pr.state = "closed"
                pr.closed_reason = f"base branch {pr.base_ref} was deleted"
            elif pr.head_ref not in heads:
                pr.state = "closed"
                pr.closed_reason = f"head branch {pr.head_ref} was deleted"

    def stack_of(self, number: int) -> int | None:
        for stack, members in self.stacks.items():
            if number in members:
                return stack
        return None

    def active(self, members: list[int]) -> list[int]:
        return [n for n in members if self.prs[n].state == "open"]

    def pr(self, number: int) -> PullRequest:
        pr = self.prs.get(number)
        if pr is None:
            raise ApiError(404, "Not Found")
        return pr

    # -- payloads -----------------------------------------------------------

    def web(self, path: str = "") -> str:
        return f"https://github.com/{self.owner}/{self.name}{path}"

    def api(self, path: str = "") -> str:
        return f"https://api.github.test/repos/{self.owner}/{self.name}{path}"

    def user_payload(self) -> dict:
        base = "https://api.github.test/users/fake"
        return {
            "login": "fake", "id": 1, "node_id": "U_1",
            "avatar_url": "https://avatars.github.test/u/1", "gravatar_id": "",
            "url": base, "html_url": "https://github.com/fake",
            "followers_url": base + "/followers", "following_url": base + "/following",
            "gists_url": base + "/gists", "starred_url": base + "/starred",
            "subscriptions_url": base + "/subscriptions",
            "organizations_url": base + "/orgs", "repos_url": base + "/repos",
            "events_url": base + "/events", "received_events_url": base + "/received_events",
            "type": "User", "site_admin": False,
        }

    def pr_payload(self, pr: PullRequest) -> dict:
        heads = self.branches()
        n = pr.number
        head_sha = heads.get(pr.head_ref, pr.last_head_sha)
        return {
            "url": self.api(f"/pulls/{n}"), "id": 1000 + n, "node_id": f"PR_{n}",
            "html_url": self.web(f"/pull/{n}"), "diff_url": self.web(f"/pull/{n}.diff"),
            "patch_url": self.web(f"/pull/{n}.patch"), "issue_url": self.api(f"/issues/{n}"),
            "commits_url": self.api(f"/pulls/{n}/commits"),
            "review_comments_url": self.api(f"/pulls/{n}/comments"),
            "review_comment_url": self.api("/pulls/comments{/number}"),
            "comments_url": self.api(f"/issues/{n}/comments"),
            "statuses_url": self.api(f"/statuses/{head_sha}"),
            "number": n, "state": pr.state, "locked": False, "title": pr.title,
            "user": self.user_payload(), "body": pr.body, "labels": [],
            "created_at": "2026-01-01T00:00:00Z", "updated_at": _now(),
            "closed_at": None if pr.state == "open" else _now(),
            "merged": pr.merged_at is not None, "merged_at": pr.merged_at,
            "merge_commit_sha": pr.merge_commit_sha,
            "assignees": [], "requested_reviewers": [], "requested_teams": [],
            "head": {"label": f"{self.owner}:{pr.head_ref}", "ref": pr.head_ref, "sha": head_sha},
            "base": {"label": f"{self.owner}:{pr.base_ref}", "ref": pr.base_ref,
                     "sha": heads.get(pr.base_ref, "")},
            "_links": {}, "author_association": "OWNER", "draft": pr.draft,
            "additions": 0, "deletions": 0, "changed_files": 0, "commits": 1,
            "review_comments": 0, "comments": 0,
        }

    def stack_payload(self, number: int) -> dict:
        members = self.stacks[number]
        heads = self.branches()
        return {
            "number": number,
            "pull_requests": [
                {
                    "number": n,
                    "state": self.prs[n].state,
                    "merged_at": self.prs[n].merged_at,
                    "head": {"ref": self.prs[n].head_ref,
                             "sha": heads.get(self.prs[n].head_ref, self.prs[n].last_head_sha)},
                }
                for n in members
            ],
        }

    def graphql_pr(self, pr: PullRequest) -> dict:
        state = "MERGED" if pr.merged_at else pr.state.upper()
        return {
            "number": pr.number,
            "state": state,
            "reviewDecision": None,
            "title": pr.title,
            "body": pr.body,
            "baseRefName": pr.base_ref,
            "headRefName": pr.head_ref,
            "headRefOid": self.branches().get(pr.head_ref, pr.last_head_sha),
            "mergeable": "MERGEABLE",
            "mergeCommit": {"oid": pr.merge_commit_sha} if pr.merge_commit_sha else None,
            "latestOpinionatedReviews": {"nodes": []},
            "reviewRequests": {"nodes": []},
        }

    # -- stacks ---------------------------------------------------------------

    def validate_stack(self, *, admitted: list[int], chained: list[int], allowed_stack=None):
        if len(set(chained)) != len(chained):
            raise ApiError(422, "Duplicate pull request.")
        for n in admitted:
            if n not in self.prs:
                raise ApiError(422, "Pull request does not exist.")
            if self.prs[n].state != "open":
                raise ApiError(422, "Pull request is not admissible.")
        for below, above in zip(chained, chained[1:]):
            if self.prs[above].base_ref != self.prs[below].head_ref:
                raise ApiError(422, "Pull request bases do not form a chain.")
        for number, members in self.stacks.items():
            if number != allowed_stack and set(members) & set(admitted):
                raise ApiError(422, "Pull request already belongs to a stack.")

    def create_stack(self, members: list[int]) -> dict:
        if len(members) < 2:
            raise ApiError(422, "A stack requires two pull requests.")
        self.validate_stack(admitted=members, chained=members)
        number = self.next_stack
        self.next_stack += 1
        self.stacks[number] = list(members)
        return self.stack_payload(number)

    def add_to_stack(self, number: int, added: list[int]) -> dict:
        if number not in self.stacks:
            raise ApiError(404, "Not Found")
        if not added:
            raise ApiError(422, "No pull requests to append.")
        existing = self.stacks[number]
        self.validate_stack(
            admitted=added, chained=self.active(existing) + added, allowed_stack=number
        )
        self.stacks[number] = existing + added
        return self.stack_payload(number)

    def unstack(self, number: int) -> dict | None:
        if number not in self.stacks:
            raise ApiError(404, "Not Found")
        retained = [n for n in self.stacks[number] if self.prs[n].merged_at]
        if retained:
            self.stacks[number] = retained
            return self.stack_payload(number)
        del self.stacks[number]
        return None

    # -- merging ---------------------------------------------------------------

    def squash_merge(self, pr: PullRequest) -> str:
        heads = self.branches()
        base, head = heads[pr.base_ref], heads[pr.head_ref]
        tree = self.git("merge-tree", "--write-tree", base, head).splitlines()[0]
        commit = self.git("commit-tree", tree, "-p", base, "-m", f"{pr.title} (#{pr.number})")
        self.git("update-ref", f"refs/heads/{pr.base_ref}", commit)
        pr.merged_at = _now()
        pr.merge_commit_sha = commit
        pr.last_head_sha = head
        pr.state = "closed"
        return commit

    def rebase_onto(self, pr: PullRequest, *, old_base: str, new_base_ref: str) -> None:
        """GitHub rebasing a surviving stack member after a merge below it."""
        heads = self.branches()
        new_base = heads[new_base_ref]
        head = heads[pr.head_ref]
        tree = self.git(
            "merge-tree", "--write-tree", f"--merge-base={old_base}", new_base, head
        ).splitlines()[0]
        commit = self.git("commit-tree", tree, "-p", new_base, "-m", pr.title)
        self.git("update-ref", f"refs/heads/{pr.head_ref}", commit)
        pr.base_ref = new_base_ref
        pr.last_head_sha = commit

    def complete_stack_merge(self, op: MergeOperation) -> None:
        stack = self.stack_of(op.pr_number)
        members = self.active(self.stacks[stack]) if stack else [op.pr_number]
        prefix = members[: members.index(op.pr_number) + 1]
        survivors = members[len(prefix):]
        base_ref = self.prs[prefix[0]].base_ref
        heads = self.branches()
        old_top = heads[self.prs[prefix[-1]].head_ref]
        for number in prefix:
            pr = self.prs[number]
            pr.base_ref = base_ref
            self.squash_merge(pr)
        previous_base, previous_old = base_ref, old_top
        for number in survivors:
            pr = self.prs[number]
            old_head = self.branches()[pr.head_ref]
            self.rebase_onto(pr, old_base=previous_old, new_base_ref=previous_base)
            previous_base, previous_old = pr.head_ref, old_head
        op.status = "merged"
        op.message = "Pull request successfully merged."
        op.sha = self.branches()[base_ref]


def _base_env() -> dict[str, str]:
    import os

    return dict(os.environ)


def _merge_payload(op: MergeOperation) -> dict:
    return {
        "status": op.status,
        "details": {
            "expected_head_sha": op.expected_head,
            "merge_action": "direct_merge",
            "merge_method": op.method,
            "message": op.message,
            "sha": op.sha,
            "uuid": op.uuid,
        },
    }


def _handler(state: FakeGitHub):
    repo_prefix = f"/repos/{state.owner}/{state.name}"

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args) -> None:  # keep test output quiet
            pass

        def _body(self) -> object:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            return json.loads(raw) if raw else None

        def _send(self, status: int, payload: object | None) -> None:
            data = b"" if payload is None else json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def _dispatch(self, method: str) -> None:
            url = urlparse(self.path)
            body = self._body() if method in ("POST", "PUT", "PATCH") else None
            with state.lock:
                state.requests.append((method, url.path, body))
                try:
                    state.refresh()
                    status, payload = self._route(method, url.path, parse_qs(url.query), body)
                    state.refresh()
                except ApiError as error:
                    status, payload = error.status, {"message": error.message}
            self._send(status, payload)

        do_GET = lambda self: self._dispatch("GET")  # noqa: E731
        do_POST = lambda self: self._dispatch("POST")  # noqa: E731
        do_PUT = lambda self: self._dispatch("PUT")  # noqa: E731
        do_PATCH = lambda self: self._dispatch("PATCH")  # noqa: E731

        def _route(self, method, path, query, body):
            if path == "/graphql" and method == "POST":
                return 200, self._graphql(body)
            if not path.startswith(repo_prefix):
                raise ApiError(404, "Not Found")
            rest = path[len(repo_prefix):]

            if rest.startswith("/stacks"):
                if not state.stacks_enabled:
                    raise ApiError(404, "Not Found")
                return self._stacks(method, rest, query, body)

            if method == "POST" and rest == "/pulls":
                heads = state.branches()
                for key in ("head", "base"):
                    if body[key] not in heads:
                        raise ApiError(422, f"Branch {body[key]!r} does not exist.")
                pr = PullRequest(
                    number=state.next_number, title=body["title"], body=body.get("body") or "",
                    head_ref=body["head"], base_ref=body["base"], draft=bool(body.get("draft")),
                    last_head_sha=heads[body["head"]],
                )
                state.next_number += 1
                state.prs[pr.number] = pr
                return 201, state.pr_payload(pr)

            m = re.fullmatch(r"/pulls/(\d+)", rest)
            if m:
                pr = state.pr(int(m.group(1)))
                if method == "GET":
                    return 200, state.pr_payload(pr)
                if method == "PATCH":
                    return 200, self._update_pr(pr, body)

            m = re.fullmatch(r"/pulls/(\d+)/requested_reviewers", rest)
            if m and method == "POST":
                return 201, state.pr_payload(state.pr(int(m.group(1))))

            m = re.fullmatch(r"/pulls/(\d+)/merge", rest)
            if m and method == "PUT":
                pr = state.pr(int(m.group(1)))
                if state.stack_of(pr.number) is not None:
                    raise ApiError(405, "Stacked pull requests must be merged as a stack.")
                if body.get("sha") and body["sha"] != state.branches().get(pr.head_ref):
                    raise ApiError(409, "Head branch was modified.")
                sha = state.squash_merge(pr)
                return 200, {"sha": sha, "merged": True, "message": "Pull Request successfully merged"}

            m = re.fullmatch(r"/pulls/(\d+)/merge-async", rest)
            if m and method == "PUT":
                pr = state.pr(int(m.group(1)))
                if body.get("merge_action") != "direct_merge":
                    raise ApiError(400, "Merge action does not match policy.")
                if state.branches().get(pr.head_ref) != body.get("sha"):
                    raise ApiError(400, "Target head changed.")
                stack = state.stack_of(pr.number)
                active = state.active(state.stacks[stack]) if stack else [pr.number]
                if pr.number not in active or pr.draft:
                    raise ApiError(400, "Target is not mergeable.")
                op = MergeOperation(
                    uuid=f"merge-{len(state.merge_ops) + 1}", pr_number=pr.number,
                    method=body.get("merge_method") or "merge", expected_head=body["sha"],
                )
                state.merge_ops[pr.number] = op
                return 202, _merge_payload(op)

            m = re.fullmatch(r"/pulls/(\d+)/merge-async/([\w-]+)", rest)
            if m and method == "GET":
                op = state.merge_ops.get(int(m.group(1)))
                if op is None or op.uuid != m.group(2):
                    raise ApiError(404, "Not Found")
                if op.status == "pending":
                    state.complete_stack_merge(op)
                return 200, _merge_payload(op)

            raise ApiError(404, "Not Found")

        def _update_pr(self, pr: PullRequest, body: dict) -> dict:
            if "base" in body and body["base"] != pr.base_ref:
                if state.stack_of(pr.number) is not None:
                    raise ApiError(422, "A stacked pull request's base cannot be updated directly.")
                if pr.state != "open":
                    raise ApiError(422, "Cannot change the base branch of a closed pull request.")
                if body["base"] not in state.branches():
                    raise ApiError(422, f"Branch {body['base']!r} does not exist.")
                pr.base_ref = body["base"]
            if "title" in body:
                pr.title = body["title"]
            if "body" in body:
                pr.body = body["body"] or ""
            if body.get("state") == "closed" and pr.state == "open":
                pr.state = "closed"
                pr.closed_reason = "closed by request"
            return state.pr_payload(pr)

        def _stacks(self, method, rest, query, body):
            if method == "GET" and rest == "/stacks":
                page = int((query.get("page") or ["1"])[0])
                numbers = sorted(state.stacks) if page == 1 else []
                if "pull_request" in query:
                    wanted = int(query["pull_request"][0])
                    numbers = [n for n in numbers if wanted in state.stacks[n]]
                return 200, [state.stack_payload(n) for n in numbers]
            if method == "POST" and rest == "/stacks":
                return 201, state.create_stack(list(body["pull_requests"]))
            m = re.fullmatch(r"/stacks/(\d+)", rest)
            if m and method == "GET":
                number = int(m.group(1))
                if number not in state.stacks:
                    raise ApiError(404, "Not Found")
                return 200, state.stack_payload(number)
            m = re.fullmatch(r"/stacks/(\d+)/add", rest)
            if m and method == "POST":
                return 200, state.add_to_stack(int(m.group(1)), list(body["pull_requests"]))
            m = re.fullmatch(r"/stacks/(\d+)/unstack", rest)
            if m and method == "POST":
                remaining = state.unstack(int(m.group(1)))
                return (204, None) if remaining is None else (200, remaining)
            raise ApiError(404, "Not Found")

        def _graphql(self, body: dict) -> dict:
            name = body.get("operationName") or ""
            variables = body.get("variables") or {}
            if name in ("PullRequestQuery", "PullRequestMergeabilityQuery"):
                pr = state.prs.get(int(variables["number"]))
                data = None if pr is None else state.graphql_pr(pr)
                return {"data": {"repository": {"pullRequest": data}}}
            if name == "OpenPullRequestBranchesQuery":
                nodes = [
                    {"number": pr.number, "headRefName": pr.head_ref, "baseRefName": pr.base_ref}
                    for pr in state.prs.values()
                    if pr.state == "open"
                ]
                return {"data": {"repository": {"pullRequests": {
                    "nodes": nodes, "pageInfo": {"hasNextPage": False, "endCursor": None}}}}}
            if name == "SearchQuery":
                nodes = [
                    {"__typename": "PullRequest", "number": pr.number, "title": pr.title,
                     "url": state.web(f"/pull/{pr.number}"), "reviewDecision": None}
                    for pr in state.prs.values()
                    if pr.state == "open"
                ]
                return {"data": {"search": {"nodes": nodes}}}
            return {"errors": [{"message": f"fake github: unknown operation {name!r}"}]}

    return Handler


def serve(state: FakeGitHub) -> tuple[ThreadingHTTPServer, str]:
    """Start the fake on a free local port; returns the server and its URL."""
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler(state))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"
