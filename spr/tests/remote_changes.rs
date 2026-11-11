//! End-to-end tests of `jj spr diff` refusing to push over commits that
//! someone else pushed to a Pull Request, against a fake GitHub (see
//! `common/fake_github.rs`). They need `jj` and `git` on `PATH`. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn heads(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.head_of(n)).collect()
}

#[test]
fn diff_refuses_to_drop_colleagues_push() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.push_to_pr_branch(prs[0], "a-fix.txt", "fix\n");
    // A local edit to the other PR must not matter.
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "--all", "-m", "update"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!(
            "#{} has commits that are not in the local change",
            prs[0]
        )),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!("jj spr patch {}", prs[0])),
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");

    env.spr(&["diff", "--all", "-m", "update", "--discard-remote-changes"]);
    env.check_chain(&changes);
    assert_eq!(
        env.pr_files(prs[0]),
        vec!["a.txt"],
        "the colleague's file is dropped"
    );
}

#[test]
fn diff_refuses_when_colleague_edits_the_same_lines() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a"]);
    let prs = env.prs_for(&changes);
    env.push_to_pr_branch(prs[0], "a.txt", "a, by a colleague\n");

    let out = env.try_spr(&["diff", "--all", "-m", "update"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("jj spr patch"), "{}", out.text);
}

#[test]
fn diff_allows_own_edits_to_the_same_lines() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    for round in 1..=2 {
        std::fs::write(env.work.join("a.txt"), format!("a, round {round}\n")).unwrap();
        env.jj(&["squash", "--into", &changes[0]]);
        env.spr(&["diff", "--all", "-m", &format!("round {round}")]);
        env.check_chain(&changes);
    }
}

#[test]
fn diff_allows_github_rebase_after_stacked_merge() {
    // After a stacked merge GitHub rebases the rest of the stack itself.
    // Those heads are nobody else's work, even with local edits since.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    env.spr(&["land", "-r", &changes[0]]);
    env.spr(&["sync"]);
    std::fs::write(env.work.join("c.txt"), "c, edited after sync\n").unwrap();
    env.jj(&["squash", "--into", &changes[2]]);

    env.spr(&["diff", "--all", "-m", "after sync"]);
    env.check_chain(&changes[1..]);
}

#[test]
fn diff_after_patch_update_pushes() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.push_to_pr_branch(prs[1], "b-fix.txt", "fix\n");

    env.spr(&["patch", &prs[1].to_string()]);
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    env.spr(&["diff", "--all", "-m", "on top of the fix"]);
    env.check_chain(&changes);
    assert_eq!(env.pr_files(prs[1]), vec!["b-fix.txt", "b.txt"]);
}

#[test]
fn diff_allows_github_rebase_onto_older_main() {
    // As above, but main moves on before the land and again before the
    // sync, so GitHub's rebased heads and the local changes are on different
    // versions of main and no local version has the same tree as GitHub's.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    env.push_to_main("before-land.txt", "1\n");
    env.spr(&["land", "-r", &changes[0]]);
    env.push_to_main("after-land.txt", "2\n");
    env.spr(&["sync"]);
    std::fs::write(env.work.join("c.txt"), "c, edited after sync\n").unwrap();
    env.jj(&["squash", "--into", &changes[2]]);

    env.spr(&["diff", "--all", "-m", "after sync"]);
    env.check_chain(&changes[1..]);
}

#[test]
fn diff_refetches_a_rewritten_pr_branch() {
    // GitHub rewrites PR branches; a plain fetch then refuses the update as a
    // non-fast-forward and diff would compare against a stale head.
    // A single PR, so only its own (head) fetch can repair the local copy.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a"]);
    let prs = env.prs_for(&changes);
    let branch = env.pr(prs[0]).head_ref;
    env.push_to_pr_branch(prs[0], "a-fix.txt", "fix\n");
    // Leave the local copy of the branch on an unrelated commit.
    let unrelated = env.gh.state().git(&[
        "commit-tree",
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        "-m",
        "unrelated",
    ]);
    env.jj(&["git", "fetch"]);
    env.git_in(
        &env.work.clone(),
        &[
            "fetch",
            "-q",
            env.root.path().join("remote.git").to_str().unwrap(),
            &unrelated,
        ],
    );
    env.git_in(
        &env.work.clone(),
        &[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            &unrelated,
        ],
    );

    let out = env.try_spr(&["diff", "--all", "-m", "would drop the push"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains("commits that are not in the local change"),
        "{}",
        out.text
    );
}

#[test]
fn diff_refuses_colleagues_push_after_github_rebase() {
    // The live run's sequence: land part of the stack (GitHub rewrites the
    // remaining branches), sync, resubmit, then a colleague pushes.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.spr(&["land", "-r", &changes[1], "--yes"]);
    env.spr(&["sync"]);
    env.spr(&["diff", "--all", "-m", "after landing"]);
    let e = env.commit("Change E", "e.txt", "e\n");
    env.spr(&["diff", "--all", "-m", "add E"]);
    env.push_to_pr_branch(prs[2], "c-fix.txt", "fix\n");

    let out = env.try_spr(&["diff", "--all", "-m", "would drop the push"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains("commits that are not in the local change"),
        "{}",
        out.text
    );
    let _ = e;
}

/// Rewrite the branches of `prs[1]` and `prs[2]` the way GitHub's stack
/// rebase did in oxidecomputer/propolis#1242-#1245: each branch keeps only
/// the content of the PR below it, and its own change is dropped.
fn shift_branches_down(env: &TestEnv, prs: &[u64]) {
    let a_head = env.head_of(prs[0]);
    let b_head = env.head_of(prs[1]);
    let b_branch = env.pr(prs[1]).head_ref;
    let c_branch = env.pr(prs[2]).head_ref;
    let b_lossy = env.remote_git(&[
        "commit-tree",
        &format!("{a_head}^{{tree}}"),
        "-p",
        &a_head,
        "-m",
        "[spr] changes to main this commit is based on",
    ]);
    let c_lossy = env.remote_git(&[
        "commit-tree",
        &format!("{b_head}^{{tree}}"),
        "-p",
        &b_lossy,
        "-m",
        "[spr] changes to main this commit is based on",
    ]);
    env.remote_git(&["update-ref", &format!("refs/heads/{b_branch}"), &b_lossy]);
    env.remote_git(&["update-ref", &format!("refs/heads/{c_branch}"), &c_lossy]);
    // `jj spr sync` fetches, so the rewritten heads are local.
    env.jj(&["git", "fetch"]);
}

/// A stack where C removes a line B adds, so B's diff is not in C.
fn build_stack_where_c_undoes_part_of_b(env: &TestEnv) -> Vec<String> {
    let a = env.commit("Change A", "shared.txt", "one\ntwo\n");
    let b = env.commit("Change B", "shared.txt", "one\nnote\ntwo\n");
    let c = env.commit("Change C", "shared.txt", "one\ntwo\n");
    env.spr(&["diff", "--all", "-m", "initial"]);
    // B moves on after C exists (as #1243 did), so no earlier version of C
    // contains what the rewritten branch holds: the first version of C is
    // empty on B, so its tree is B's as it was then. (Real GitHub replays
    // B's edit into B's branch too, which this shortcut leaves out; see
    // `diff_after_github_restacks_a_converted_stack` for the real shape.)
    std::fs::write(env.work.join("b.txt"), "b\n").unwrap();
    env.jj(&["squash", "--into", &b]);
    env.spr(&["diff", "--all", "-m", "edit B"]);
    vec![a, b, c]
}

#[test]
fn diff_allows_branch_holding_only_the_parents_content() {
    // GitHub's restack left #1244 holding #1243's diff, which adds lines
    // #1244 deletes. That diff is all in the local parent, so pushing over
    // it drops nothing.
    let env = TestEnv::new();
    let changes = build_stack_where_c_undoes_part_of_b(&env);
    let prs = env.prs_for(&changes);
    shift_branches_down(&env, &prs);

    env.spr(&["diff", "--all", "-m", "after GitHub's restack"]);
    env.check_chain(&changes);
}

#[test]
fn diff_refuses_colleagues_push_on_top_of_parents_content() {
    // As above, but someone also pushed to C: that is still their work.
    let env = TestEnv::new();
    let changes = build_stack_where_c_undoes_part_of_b(&env);
    let prs = env.prs_for(&changes);
    shift_branches_down(&env, &prs);
    env.push_to_pr_branch(prs[2], "c-fix.txt", "fix\n");
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "--all", "-m", "would drop the push"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!(
            "#{} has commits that are not in the local change",
            prs[2]
        )),
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

/// Make `change` a commit that jj imported from Git, with no history in
/// `jj evolog`, as for a change created on another machine. Returns its
/// new change ID.
fn reimport_without_evolog(env: &TestEnv, change: &str) -> String {
    let commit = env.commit_id(change);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&env.work)
            .args(args)
            .envs([
                ("GIT_AUTHOR_NAME", "Test"),
                ("GIT_AUTHOR_EMAIL", "test@example.com"),
                ("GIT_COMMITTER_NAME", "Test"),
                ("GIT_COMMITTER_EMAIL", "test@example.com"),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let message = git(&["log", "-1", "--format=%B", &commit]);
    let copy = git(&[
        "commit-tree",
        &format!("{commit}^{{tree}}"),
        "-p",
        &format!("{commit}^"),
        "-m",
        &message,
    ]);
    git(&["update-ref", "refs/heads/reimport", &copy]);
    env.jj(&["git", "import"]);
    env.jj(&["abandon", change]);
    env.jj(&["new", &copy]);
    env.jj(&["bookmark", "forget", "reimport"]);
    let id = env.change_id(&copy);
    assert_eq!(
        env.jj_query(&[
            "evolog",
            "--no-graph",
            "-r",
            &id,
            "-T",
            "commit.commit_id() ++ \"\\n\""
        ])
        .lines()
        .count(),
        1,
        "the copy should have no earlier versions"
    );
    id
}

/// The propolis #1241-#1245 history: Pull Requests opened without native
/// stacks, so each one's own change is in a merge commit, then made into a
/// stack. B moves on after C exists, and C removes a line that B adds.
/// Returns the changes once `main` has moved and GitHub's stack rebase
/// (which drops merge commits) has left C's branch with only B's content.
fn converted_stack_after_github_restack(env: &TestEnv, without_evolog: bool) -> Vec<String> {
    env.push_to_main("shared.txt", "one\ntwo\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["new", "main@origin"]);
    let a = env.commit("Change A", "a.txt", "a\n");
    std::fs::write(env.work.join("b.txt"), "b\n").unwrap();
    let b = env.commit("Change B", "shared.txt", "one\nnote\ntwo\n");
    std::fs::write(env.work.join("c.txt"), "c\n").unwrap();
    let mut c = env.commit("Change C", "shared.txt", "one\ntwo\n");
    if without_evolog {
        c = reimport_without_evolog(env, &c);
    }
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "initial"]);
    std::fs::write(env.work.join("b2.txt"), "b2\n").unwrap();
    env.jj(&["squash", "--into", &b]);
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "edit B"]);
    env.spr(&["diff", "--all", "--native-stack", "-m", "make them a stack"]);
    let changes = vec![a, b, c];
    let prs = env.prs_for(&changes);

    env.push_to_main("other.txt", "other\n");
    env.gh.state().rebase_stack_on_github(prs[0]);
    // What GitHub did to propolis: C's branch holds B's content, not C's.
    let c_head = env.head_of(prs[2]);
    let files = env.remote_git(&["ls-tree", "--name-only", &c_head]);
    assert!(!files.lines().any(|f| f == "c.txt"), "{files}");
    assert_eq!(
        env.remote_git(&["show", &format!("{c_head}:shared.txt")]),
        "one\nnote\ntwo"
    );
    env.spr(&["sync"]);
    changes
}

#[test]
fn diff_after_github_restacks_a_converted_stack() {
    let env = TestEnv::new();
    let changes = converted_stack_after_github_restack(&env, false);
    env.spr(&["diff", "--all", "-m", "after GitHub's restack"]);
    let prs = env.check_chain(&changes);
    assert_eq!(env.pr_files(prs[2]), vec!["c.txt", "shared.txt"]);
}

#[test]
fn diff_after_github_restacks_a_change_with_no_evolog() {
    // As above, but C came from elsewhere, so no earlier version of it holds
    // B's content: only the local parent shows that pushing loses nothing.
    let env = TestEnv::new();
    let changes = converted_stack_after_github_restack(&env, true);
    env.spr(&["diff", "--all", "-m", "after GitHub's restack"]);
    let prs = env.check_chain(&changes);
    assert_eq!(env.pr_files(prs[2]), vec!["c.txt", "shared.txt"]);
}
