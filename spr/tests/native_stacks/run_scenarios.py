#!/usr/bin/env python3
"""End-to-end checks of jj-spr's native GitHub stacks against a fake GitHub.

Usage: run_scenarios.py [--jj-spr PATH] [--jj PATH] [-k NAME] [-v]

Each scenario builds a fresh jj repository whose `origin` is a bare Git
repository served by fake_github.py, runs the real `jj` and `jj-spr`
binaries, and checks the resulting pull requests, branches and stacks.
Only the Python standard library is needed (3.9+).
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from fake_github import FakeGitHub, serve  # noqa: E402

VERBOSE = False


class Env:
    """One jj repository wired to a fresh fake GitHub."""

    def __init__(self, root: Path, jj_spr: str, jj: str, stacks_enabled: bool = True):
        self.root, self.jj_spr, self.jj_bin = root, jj_spr, jj
        self.remote = root / "remote.git"
        self.work = root / "work"
        self.env = {
            **os.environ,
            "JJ_CONFIG": str(root / "jj-config.toml"),
            "JJ_USER": "Test User",
            "JJ_EMAIL": "test@example.com",
            "GIT_AUTHOR_NAME": "Test User",
            "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test User",
            "GIT_COMMITTER_EMAIL": "test@example.com",
            "GIT_CONFIG_GLOBAL": str(root / "gitconfig"),
            "NO_COLOR": "1",
        }
        (root / "jj-config.toml").write_text("")
        (root / "gitconfig").write_text("")

        run(["git", "init", "--bare", "-b", "main", str(self.remote)], env=self.env)
        seed = root / "seed"
        run(["git", "clone", str(self.remote), str(seed)], env=self.env)
        (seed / "README").write_text("hello\n")
        run(["git", "add", "README"], cwd=seed, env=self.env)
        run(["git", "commit", "-m", "Initial commit"], cwd=seed, env=self.env)
        run(["git", "push", "origin", "main"], cwd=seed, env=self.env)

        self.gh = FakeGitHub(owner="fake", name="repo", git_dir=self.remote,
                             stacks_enabled=stacks_enabled)
        self.server, url = serve(self.gh)

        run([jj, "git", "clone", "--colocate", str(self.remote), str(self.work)], env=self.env)
        for key, value in [
            ("spr.githubRepository", "fake/repo"),
            ("spr.branchPrefix", "spr/"),
            ("spr.githubAuthToken", "fake-token"),
            ("spr.githubApiUrl", url),
            ("spr.nativeStacks", "true"),
        ]:
            self.jj("config", "set", "--repo", key, value)

    def close(self) -> None:
        self.server.shutdown()

    # -- commands --------------------------------------------------------

    def jj(self, *args: str) -> str:
        return run([self.jj_bin, *args], cwd=self.work, env=self.env)

    def jj_query(self, *args: str) -> str:
        """Run jj for its output only (without notices it prints on stderr)."""
        return run([self.jj_bin, *args], cwd=self.work, env=self.env, stdout_only=True)

    def spr(self, *args: str, check: bool = True) -> str:
        return run([self.jj_spr, *args], cwd=self.work, env=self.env, check=check)

    def commit(self, title: str, path: str, content: str) -> str:
        """Create a change on top of @- (the working copy's parent); returns its change ID."""
        (self.work / path).write_text(content)
        self.jj("commit", "-m", title)
        return self.change_id("@-")

    def change_id(self, rev: str) -> str:
        return self.jj_query("log", "--no-graph", "-r", rev, "-T", "change_id").strip()

    def commit_id(self, rev: str) -> str:
        return self.jj_query("log", "--no-graph", "-r", rev, "-T", "commit_id").strip()

    # -- observations ----------------------------------------------------

    def pr_for(self, change: str) -> int:
        description = self.jj_query("log", "--no-graph", "-r", change, "-T", "description")
        for line in description.splitlines():
            if line.startswith("Pull Request:"):
                return int(line.rsplit("/", 1)[1])
        raise AssertionError(f"change {change} has no Pull Request trailer:\n{description}")

    def branch(self, name: str) -> str | None:
        return self.gh.branches().get(name)

    def pr_files(self, number: int) -> list[str]:
        """Files in GitHub's diff for a PR: changes on head since its merge base with base."""
        pr = self.gh.prs[number]
        heads = self.gh.branches()
        base, head = heads[pr.base_ref], heads[pr.head_ref]
        merge_base = self.gh.git("merge-base", base, head)
        out = self.gh.git("diff", "--name-only", merge_base, head)
        return sorted(out.split())

    def stacks(self) -> dict[int, list[int]]:
        return {n: list(m) for n, m in self.gh.stacks.items()}

    def active_stacks(self) -> list[list[int]]:
        return [a for a in (self.gh.active(m) for m in self.gh.stacks.values()) if a]

    def requests(self, method: str, fragment: str) -> list:
        return [r for r in self.gh.requests if r[0] == method and fragment in r[1]]


def run(cmd, cwd=None, env=None, check=True, stdout_only=False) -> str:
    done = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True,
                          stdin=subprocess.DEVNULL)
    if VERBOSE:
        print(f"$ {' '.join(map(str, cmd))}\n{done.stdout}{done.stderr}")
    if check and done.returncode != 0:
        raise AssertionError(
            f"command failed ({done.returncode}): {' '.join(map(str, cmd))}\n"
            f"stdout:\n{done.stdout}\nstderr:\n{done.stderr}"
        )
    return done.stdout if stdout_only else done.stdout + done.stderr


def expect(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def build_stack(env: Env, names=("a", "b", "c")) -> list[str]:
    changes = [env.commit(f"Change {n.upper()}", f"{n}.txt", f"{n}\n") for n in names]
    env.spr("diff", "--all", "-m", "initial")
    return changes


def check_chain(env: Env, changes: list[str]) -> list[int]:
    """Every PR targets the one below, shows only its own file, and they form one stack."""
    prs = [env.pr_for(c) for c in changes]
    for i, number in enumerate(prs):
        pr = env.gh.prs[number]
        expected_base = "main" if i == 0 else env.gh.prs[prs[i - 1]].head_ref
        expect(pr.state == "open", f"PR #{number} is {pr.state} ({pr.closed_reason})")
        expect(pr.base_ref == expected_base,
               f"PR #{number} targets {pr.base_ref}, expected {expected_base}")
        own = env.jj_query("diff", "--name-only", "-r", changes[i]).split()
        expect(env.pr_files(number) == sorted(own),
               f"PR #{number} shows {env.pr_files(number)}, expected {sorted(own)}")
        head_tree = env.gh.git("rev-parse", f"{env.branch(pr.head_ref)}^{{tree}}")
        local_tree = env.jj_query("log", "--no-graph", "-r", changes[i], "-T", "commit_id")
        local_tree = run(["git", "rev-parse", f"{local_tree.strip()}^{{tree}}"],
                         cwd=env.work, env=env.env).strip()
        expect(head_tree == local_tree, f"PR #{number}'s tree differs from the local change")
    expect(prs in env.active_stacks(), f"expected stack {prs}, have {env.stacks()}")
    return prs


# -- scenarios -----------------------------------------------------------------


def scenario_create_stack(env: Env) -> None:
    changes = build_stack(env)
    prs = check_chain(env, changes)
    expect(not any(b.startswith("spr/main.") for b in env.gh.branches()),
           "native mode should not create spr/main.* base branches")
    expect(len(env.requests("POST", "/stacks")) == 1, "expected one stack creation")
    expect(prs == [1, 2, 3], f"unexpected PR numbers {prs}")


def scenario_rerun_is_noop(env: Env) -> None:
    changes = build_stack(env)
    heads = env.gh.branches()
    before = len(env.gh.requests)
    out = env.spr("diff", "--all", "-m", "again")
    expect(env.gh.branches() == heads, "rerun changed branches")
    expect("No update necessary" in out, out)
    mutations = [r for r in env.gh.requests[before:] if r[0] != "GET" and r[1] != "/graphql"]
    expect(not mutations, f"rerun made changes: {mutations}")
    check_chain(env, changes)


def scenario_amend_middle(env: Env) -> None:
    changes = build_stack(env)
    prs = [env.pr_for(c) for c in changes]
    old_heads = {n: env.branch(env.gh.prs[n].head_ref) for n in prs}
    env.jj("edit", changes[1])
    (env.work / "b.txt").write_text("b, revised\n")
    env.jj("new", changes[2])
    env.spr("diff", "--all", "-m", "review feedback")
    check_chain(env, changes)
    expect(env.branch(env.gh.prs[prs[0]].head_ref) == old_heads[prs[0]],
           "the bottom PR should not change")
    for n in prs[1:]:
        new = env.branch(env.gh.prs[n].head_ref)
        expect(new != old_heads[n], f"PR #{n} was not updated")
        expect(env.gh.is_ancestor(old_heads[n], new),
               f"PR #{n} lost its history (update commits should be appended)")
    message = env.gh.git("log", "-1", "--format=%s", env.branch(env.gh.prs[prs[1]].head_ref))
    expect(message == "review feedback", f"update commit message was {message!r}")


def scenario_rebase_on_main(env: Env) -> None:
    changes = build_stack(env)
    prs = [env.pr_for(c) for c in changes]
    old_heads = {n: env.branch(env.gh.prs[n].head_ref) for n in prs}
    # Someone else lands work on main.
    seed = env.root / "seed"
    run(["git", "pull", "-q", "origin", "main"], cwd=seed, env=env.env)
    (seed / "other.txt").write_text("other\n")
    run(["git", "add", "other.txt"], cwd=seed, env=env.env)
    run(["git", "commit", "-q", "-m", "Other work"], cwd=seed, env=env.env)
    run(["git", "push", "-q", "origin", "main"], cwd=seed, env=env.env)
    env.jj("git", "fetch")
    env.jj("rebase", "-s", changes[0], "-d", "main@origin")
    before = len(env.gh.requests)
    env.spr("diff", "--all", "-m", "rebase on main")
    check_chain(env, changes)
    main = env.branch("main")
    for n in prs:
        new = env.branch(env.gh.prs[n].head_ref)
        expect(new != old_heads[n], f"PR #{n} was not updated")
        expect(env.gh.is_ancestor(old_heads[n], new),
               f"PR #{n} was rewritten instead of appended to")
        expect(env.gh.is_ancestor(main, new), f"PR #{n} does not include the new main")
        parents = env.gh.git("log", "-1", "--format=%P", new).split()
        expect(parents[0] == old_heads[n], f"PR #{n}'s new commit is not on its old head")
    expect(env.pr_files(prs[0]) == ["a.txt"], env.pr_files(prs[0]))
    expect(not [r for r in env.gh.requests[before:] if "/stacks" in r[1] and r[0] == "POST"],
           "a rebase should not touch the stack")


def scenario_append_to_stack(env: Env) -> None:
    changes = build_stack(env)
    changes.append(env.commit("Change D", "d.txt", "d\n"))
    env.spr("diff", "--all", "-m", "add d")
    prs = check_chain(env, changes)
    expect(len(env.stacks()) == 1, f"expected one stack, have {env.stacks()}")
    expect(any("/add" in r[1] for r in env.gh.requests), "expected an append to the stack")
    expect(len(prs) == 4, str(prs))


def scenario_single_change_mode(env: Env) -> None:
    changes = build_stack(env)
    changes.append(env.commit("Change D", "d.txt", "d\n"))
    env.spr("diff", "-r", "@-")
    check_chain(env, changes)


def scenario_single_change_needs_parent_pr(env: Env) -> None:
    env.commit("Change A", "a.txt", "a\n")
    env.commit("Change B", "b.txt", "b\n")
    out = env.spr("diff", "-r", "@-", check=False)
    expect("has no Pull Request yet" in out, out)
    expect(not env.gh.prs, "no PR should have been created")


def scenario_reorder(env: Env) -> None:
    changes = build_stack(env, ("a", "b", "c", "d"))
    old = [env.pr_for(c) for c in changes]
    # Move D below C: a, b, d, c
    env.jj("rebase", "-r", changes[3], "--insert-before", changes[2])
    order = [changes[0], changes[1], changes[3], changes[2]]
    out = env.spr("diff", "--all", "-m", "reorder")
    expect("Dissolving GitHub stack" in out, out)
    prs = check_chain(env, order)
    expect(prs == [old[0], old[1], old[3], old[2]], f"PRs should keep their numbers: {prs}")


def scenario_insert_in_middle(env: Env) -> None:
    changes = build_stack(env)
    old = [env.pr_for(c) for c in changes]
    old_heads = {n: env.branch(env.gh.prs[n].head_ref) for n in old}
    # Insert X between B and C.
    env.jj("new", "--insert-after", changes[1], "-m", "Change X")
    (env.work / "x.txt").write_text("x\n")
    inserted = env.change_id("@")
    env.jj("new", changes[2])
    out = env.spr("diff", "--all", "-m", "insert x")
    expect("Dissolving GitHub stack" in out, out)
    order = [changes[0], changes[1], inserted, changes[2]]
    prs = check_chain(env, order)
    expect([prs[0], prs[1], prs[3]] == old, f"existing PRs should keep their numbers: {prs}")
    expect(env.branch(env.gh.prs[old[0]].head_ref) == old_heads[old[0]], "PR A changed")
    expect(env.branch(env.gh.prs[old[1]].head_ref) == old_heads[old[1]], "PR B changed")
    c_head = env.branch(env.gh.prs[old[2]].head_ref)
    expect(env.gh.is_ancestor(old_heads[old[2]], c_head), "PR C was rewritten")
    expect(len(env.active_stacks()) == 1, f"expected one stack: {env.stacks()}")


def scenario_insert_single_change(env: Env) -> None:
    changes = build_stack(env)
    old = [env.pr_for(c) for c in changes]
    env.jj("new", "--insert-after", changes[1], "-m", "Change X")
    (env.work / "x.txt").write_text("x\n")
    inserted = env.change_id("@")
    env.jj("new", changes[2])
    # Submitting only X cannot retarget C, so it must say so and change nothing else.
    out = env.spr("diff", "-r", inserted)
    x = env.pr_for(inserted)
    expect(env.gh.prs[x].base_ref == env.gh.prs[old[1]].head_ref, "X should target B")
    expect(env.gh.prs[old[2]].base_ref == env.gh.prs[old[1]].head_ref, "C should be untouched")
    flat = " ".join(out.split())  # jj-spr wraps its messages
    expect("jj spr diff --all" in flat, f"expected advice to rebuild:\n{out}")
    expect(env.active_stacks() == [old], f"stack should be unchanged: {env.stacks()}")
    # Following the advice fixes it.
    env.spr("diff", "--all", "-m", "restack")
    check_chain(env, [changes[0], changes[1], inserted, changes[2]])


def scenario_migrate_legacy_stack(env: Env) -> None:
    changes = [env.commit("Change A", "a.txt", "a\n"), env.commit("Change B", "b.txt", "b\n")]
    env.spr("diff", "--all", "--no-native-stack", "-m", "legacy")
    legacy_base = env.gh.prs[env.pr_for(changes[1])].base_ref
    expect(legacy_base.startswith("spr/main."), f"legacy base was {legacy_base}")
    expect(not env.stacks(), "legacy mode must not create stacks")
    env.spr("diff", "--all", "-m", "go native")
    check_chain(env, changes)


def scenario_land_bottom(env: Env) -> None:
    changes = build_stack(env)
    prs = [env.pr_for(c) for c in changes]
    bottom_branch = env.gh.prs[prs[0]].head_ref
    env.spr("land", "-r", changes[0])
    expect(env.gh.prs[prs[0]].merged_at is not None, "bottom PR was not merged")
    expect(any("merge-async" in r[1] for r in env.gh.requests), "expected a stack merge")
    for n in prs[1:]:
        pr = env.gh.prs[n]
        expect(pr.state == "open", f"PR #{n} was closed: {pr.closed_reason}")
    expect(env.gh.prs[prs[1]].base_ref == "main", "next PR should now target main")
    expect(env.branch(bottom_branch) is None, "landed PR's branch should be deleted")

    # Bring the local stack up to date and resubmit.
    env.jj("git", "fetch")
    env.jj("rebase", "-s", changes[1], "-d", "main@origin")
    env.jj("abandon", changes[0])
    env.spr("diff", "--all", "-m", "after landing")
    check_chain(env, changes[1:])
    expect(env.pr_files(prs[1]) == ["b.txt"], env.pr_files(prs[1]))


def scenario_land_refuses_middle(env: Env) -> None:
    changes = build_stack(env)
    out = env.spr("land", "-r", changes[1], check=False)
    expect("not at the bottom" in out, out)
    expect(all(pr.merged_at is None for pr in env.gh.prs.values()), "nothing should merge")


def scenario_close_keeps_dependents(env: Env) -> None:
    changes = build_stack(env)
    prs = [env.pr_for(c) for c in changes]
    middle_branch = env.gh.prs[prs[1]].head_ref
    # Close the middle PR only: its branch must survive, because #3 targets it.
    env.gh.stacks.clear()  # GitHub's rules for closing stacked PRs are not modelled
    env.spr("close", "-r", changes[1])
    expect(env.gh.prs[prs[1]].state == "closed", "middle PR should be closed")
    expect(env.branch(middle_branch) is not None, "branch of the closed PR was deleted")
    expect(env.gh.prs[prs[2]].state == "open",
           f"dependent PR was closed: {env.gh.prs[prs[2]].closed_reason}")


def scenario_stacks_disabled(env: Env) -> None:
    changes = build_stack(env)
    prs = [env.pr_for(c) for c in changes]
    for i, n in enumerate(prs[1:], start=1):
        expect(env.gh.prs[n].base_ref == env.gh.prs[prs[i - 1]].head_ref, "PRs not chained")
        expect(env.pr_files(n) == [f"{'abc'[i]}.txt"], env.pr_files(n))
    expect(not env.stacks(), "no stacks when disabled")


SCENARIOS = {
    name.removeprefix("scenario_"): (fn, {"stacks_enabled": False} if "disabled" in name else {})
    for name, fn in list(globals().items())
    if name.startswith("scenario_")
}


def main() -> int:
    global VERBOSE
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--jj-spr", default=shutil.which("jj-spr") or "jj-spr")
    parser.add_argument("--jj", default=shutil.which("jj") or "jj")
    parser.add_argument("-k", help="only run scenarios whose name contains this")
    parser.add_argument("-v", action="store_true", help="show every command's output")
    args = parser.parse_args()
    VERBOSE = args.v

    failures = 0
    for name, (fn, options) in SCENARIOS.items():
        if args.k and args.k not in name:
            continue
        with tempfile.TemporaryDirectory(prefix=f"spr-{name}-") as tmp:
            env = Env(Path(tmp), os.path.abspath(args.jj_spr) if os.sep in args.jj_spr else args.jj_spr,
                      os.path.abspath(args.jj) if os.sep in args.jj else args.jj, **options)
            try:
                fn(env)
                print(f"ok    {name}")
            except Exception:
                failures += 1
                print(f"FAIL  {name}")
                traceback.print_exc()
            finally:
                env.close()
    print(f"\n{failures} failed" if failures else "\nall scenarios passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
