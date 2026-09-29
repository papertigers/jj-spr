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
