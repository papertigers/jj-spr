//! End-to-end tests of `jj spr diff`, `land` and `close` with native GitHub
//! stacks, against a fake GitHub (see `common/fake_github.rs`).
//!
//! These mirror `tests/native_stacks/run_scenarios.py` scenario for scenario,
//! under the same names. They need `jj` and `git` on `PATH`, like the other
//! integration tests. Set `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn strings(ids: &[&String]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn create_stack() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.check_chain(&changes);
    assert_eq!(prs, vec![1, 2, 3]);
    assert!(
        !env.gh
            .state()
            .branches()
            .keys()
            .any(|b| b.starts_with("spr/main.")),
        "native mode should not create spr/main.* base branches"
    );
    assert_eq!(
        env.requests_since(0, "POST", "/stacks").len(),
        1,
        "one stack creation"
    );
}

#[test]
fn rerun_is_noop() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let heads = env.gh.state().branches();
    let before = env.request_count();
    let out = env.spr(&["diff", "--all", "-m", "again"]);
    assert_eq!(env.gh.state().branches(), heads, "rerun changed branches");
    assert!(out.text.contains("No update necessary"), "{}", out.text);
    for method in ["POST", "PATCH", "PUT"] {
        let changes: Vec<_> = env
            .requests_since(before, method, "/repos/")
            .into_iter()
            .collect();
        assert!(changes.is_empty(), "rerun made changes: {changes:?}");
    }
    env.check_chain(&changes);
}

#[test]
fn amend_middle() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let old: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();

    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("b.txt"), "b, revised\n").unwrap();
    env.jj(&["new", &changes[2]]);
    env.spr(&["diff", "--all", "-m", "review feedback"]);

    env.check_chain(&changes);
    assert_eq!(
        env.head_of(prs[0]),
        old[0],
        "the bottom PR should not change"
    );
    for (i, &n) in prs.iter().enumerate().skip(1) {
        let new = env.head_of(n);
        assert_ne!(new, old[i], "PR #{n} was not updated");
        assert!(env.is_ancestor(&old[i], &new), "PR #{n} lost its history");
    }
    let message = env.remote_git(&["log", "-1", "--format=%s", &env.head_of(prs[1])]);
    assert_eq!(message, "review feedback");
}

#[test]
fn rebase_on_main() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let old: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();

    // Someone else lands work on main.
    env.git_in(&env.seed, &["pull", "-q", "origin", "main"]);
    std::fs::write(env.seed.join("other.txt"), "other\n").unwrap();
    env.git_in(&env.seed, &["add", "other.txt"]);
    env.git_in(&env.seed, &["commit", "-q", "-m", "Other work"]);
    env.git_in(&env.seed, &["push", "-q", "origin", "main"]);

    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    let before = env.request_count();
    env.spr(&["diff", "--all", "-m", "rebase on main"]);

    env.check_chain(&changes);
    let main = env.branch("main").unwrap();
    for (i, &n) in prs.iter().enumerate() {
        let new = env.head_of(n);
        assert_ne!(new, old[i], "PR #{n} was not updated");
        assert!(env.is_ancestor(&old[i], &new), "PR #{n} was rewritten");
        assert!(
            env.is_ancestor(&main, &new),
            "PR #{n} does not include main"
        );
        let parents = env.remote_git(&["log", "-1", "--format=%P", &new]);
        assert_eq!(
            parents.split(' ').next(),
            Some(old[i].as_str()),
            "PR #{n}'s first parent"
        );
    }
    assert_eq!(env.pr_files(prs[0]), vec!["a.txt"]);
    assert!(
        env.requests_since(before, "POST", "/stacks").is_empty(),
        "a rebase should not touch the stack"
    );
}

#[test]
fn append_to_stack() {
    let env = TestEnv::new();
    let mut changes = env.build_stack(&["a", "b", "c"]);
    changes.push(env.commit("Change D", "d.txt", "d\n"));
    let before = env.request_count();
    env.spr(&["diff", "--all", "-m", "add d"]);
    let prs = env.check_chain(&changes);
    assert_eq!(prs.len(), 4);
    assert_eq!(env.stacks().len(), 1, "{:?}", env.stacks());
    assert_eq!(
        env.requests_since(before, "POST", "/add").len(),
        1,
        "append to the stack"
    );
}

#[test]
fn single_change_mode() {
    let env = TestEnv::new();
    let mut changes = env.build_stack(&["a", "b", "c"]);
    changes.push(env.commit("Change D", "d.txt", "d\n"));
    env.spr(&["diff", "-r", "@-"]);
    env.check_chain(&changes);
}

#[test]
fn single_change_needs_parent_pr() {
    let env = TestEnv::new();
    env.commit("Change A", "a.txt", "a\n");
    env.commit("Change B", "b.txt", "b\n");
    let out = env.try_spr(&["diff", "-r", "@-"]);
    assert!(!out.success);
    assert!(
        out.flat().contains("has no Pull Request yet"),
        "{}",
        out.text
    );
    assert!(
        env.gh.state().prs.is_empty(),
        "no PR should have been created"
    );
}

#[test]
fn reorder() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c", "d"]);
    let old = env.prs_for(&changes);
    // Move D below C: a, b, d, c
    env.jj(&["rebase", "-r", &changes[3], "--insert-before", &changes[2]]);
    let out = env.spr(&["diff", "--all", "-m", "reorder"]);
    assert!(out.text.contains("Dissolving GitHub stack"), "{}", out.text);
    let prs = env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &changes[3],
        &changes[2],
    ]));
    assert_eq!(
        prs,
        vec![old[0], old[1], old[3], old[2]],
        "PRs keep their numbers"
    );
}

#[test]
fn insert_in_middle() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let old = env.prs_for(&changes);
    let old_heads: Vec<String> = old.iter().map(|&n| env.head_of(n)).collect();

    // Insert X between B and C.
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let inserted = env.change_id("@");
    env.jj(&["new", &changes[2]]);
    let out = env.spr(&["diff", "--all", "-m", "insert x"]);
    assert!(out.text.contains("Dissolving GitHub stack"), "{}", out.text);

    let prs = env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &inserted,
        &changes[2],
    ]));
    assert_eq!(
        vec![prs[0], prs[1], prs[3]],
        old,
        "existing PRs keep their numbers"
    );
    assert_eq!(env.head_of(old[0]), old_heads[0], "PR A changed");
    assert_eq!(env.head_of(old[1]), old_heads[1], "PR B changed");
    assert!(
        env.is_ancestor(&old_heads[2], &env.head_of(old[2])),
        "PR C was rewritten"
    );
    assert_eq!(env.active_stacks().len(), 1, "{:?}", env.stacks());
}

#[test]
fn insert_single_change() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let old = env.prs_for(&changes);
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let inserted = env.change_id("@");
    env.jj(&["new", &changes[2]]);

    // Submitting only X cannot retarget C, so it must say so and change nothing else.
    let out = env.spr(&["diff", "-r", &inserted]);
    let x = env.pr_for(&inserted);
    assert_eq!(
        env.pr(x).base_ref,
        env.pr(old[1]).head_ref,
        "X should target B"
    );
    assert_eq!(
        env.pr(old[2]).base_ref,
        env.pr(old[1]).head_ref,
        "C should be untouched"
    );
    assert!(out.flat().contains("jj spr diff --all"), "{}", out.text);
    assert_eq!(
        env.active_stacks(),
        vec![old.clone()],
        "stack should be unchanged"
    );

    // Following the advice fixes it.
    env.spr(&["diff", "--all", "-m", "restack"]);
    env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &inserted,
        &changes[2],
    ]));
}

#[test]
fn migrate_legacy_stack() {
    let env = TestEnv::new();
    let changes = vec![
        env.commit("Change A", "a.txt", "a\n"),
        env.commit("Change B", "b.txt", "b\n"),
    ];
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "legacy"]);
    let legacy_base = env.pr(env.pr_for(&changes[1])).base_ref;
    assert!(
        legacy_base.starts_with("spr/main."),
        "legacy base was {legacy_base}"
    );
    assert!(
        env.stacks().is_empty(),
        "legacy mode must not create stacks"
    );
    env.spr(&["diff", "--all", "-m", "go native"]);
    env.check_chain(&changes);
}

#[test]
fn land_bottom() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let bottom_branch = env.pr(prs[0]).head_ref;

    env.spr(&["land", "-r", &changes[0]]);
    assert!(
        env.pr(prs[0]).merged_at.is_some(),
        "bottom PR was not merged"
    );
    assert_eq!(
        env.requests_since(0, "PUT", "merge-async").len(),
        1,
        "a stack merge"
    );
    for &n in &prs[1..] {
        let pr = env.pr(n);
        assert!(pr.is_open(), "PR #{n} was closed: {:?}", pr.closed_reason);
    }
    assert_eq!(
        env.pr(prs[1]).base_ref,
        "main",
        "next PR should now target main"
    );
    assert!(
        env.branch(&bottom_branch).is_none(),
        "landed branch should be deleted"
    );

    // Bring the local stack up to date and resubmit.
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[1], "-d", "main@origin"]);
    env.jj(&["abandon", &changes[0]]);
    env.spr(&["diff", "--all", "-m", "after landing"]);
    env.check_chain(&changes[1..]);
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
}

#[test]
fn land_refuses_middle() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let out = env.try_spr(&["land", "-r", &changes[1]]);
    assert!(!out.success);
    assert!(out.flat().contains("not at the bottom"), "{}", out.text);
    assert!(
        env.gh.state().prs.values().all(|pr| pr.merged_at.is_none()),
        "nothing should merge"
    );
}

#[test]
fn close_keeps_dependents() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let middle_branch = env.pr(prs[1]).head_ref;
    // GitHub's rules for closing stacked PRs are not modelled.
    env.gh.state().stacks.clear();

    env.spr(&["close", "-r", &changes[1]]);
    assert!(!env.pr(prs[1]).is_open(), "middle PR should be closed");
    assert!(
        env.branch(&middle_branch).is_some(),
        "branch of the closed PR was deleted"
    );
    let dependent = env.pr(prs[2]);
    assert!(
        dependent.is_open(),
        "dependent PR was closed: {:?}",
        dependent.closed_reason
    );
}

#[test]
fn stacks_disabled() {
    let env = TestEnv::with_stacks(false);
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    for i in 1..prs.len() {
        assert_eq!(
            env.pr(prs[i]).base_ref,
            env.pr(prs[i - 1]).head_ref,
            "PRs not chained"
        );
        assert_eq!(
            env.pr_files(prs[i]),
            vec![format!("{}.txt", ["a", "b", "c"][i])]
        );
    }
    assert!(env.stacks().is_empty(), "no stacks when disabled");
}
