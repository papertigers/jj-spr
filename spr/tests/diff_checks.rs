//! End-to-end tests of what `jj spr diff` checks before it changes anything,
//! against a fake GitHub (see `common/fake_github.rs`). They need `jj` and
//! `git` on `PATH`. Set `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn heads(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.head_of(n)).collect()
}

#[test]
fn diff_refuses_conflicted_changes() {
    // papertigers/jj-spr#11: a conflicted change was pushed as jj's
    // conflict files.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.push_to_main("a.txt", "someone else's a\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "after rebase"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("conflicts"), "{}", out.text);
    assert!(out.text.contains(&changes[0][..8]), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

/// A, B (with b.txt and b2.txt) and C, submitted, then B split in two so
/// both halves keep B's `Pull Request:` line.
fn stack_with_split_middle(env: &TestEnv) -> (Vec<String>, Vec<u64>) {
    let a = env.commit("Change A", "a.txt", "a\n");
    std::fs::write(env.work.join("b2.txt"), "b2\n").unwrap();
    let b = env.commit("Change B", "b.txt", "b\n");
    let c = env.commit("Change C", "c.txt", "c\n");
    env.spr(&["diff", "--all", "-m", "initial"]);
    let changes = vec![a, b, c];
    let prs = env.prs_for(&changes);
    // Accept the description unchanged, as `jj split` does in an editor.
    env.jj(&[
        "--config",
        "ui.editor=\"true\"",
        "split",
        "-r",
        &changes[1],
        "b2.txt",
    ]);
    (changes, prs)
}

#[test]
fn diff_refuses_two_changes_with_the_same_pull_request() {
    // papertigers/jj-spr#12: after `jj split`, both halves updated the same
    // PR, which ended up based on its own branch.
    let env = TestEnv::new();
    let (_, prs) = stack_with_split_middle(&env);
    let halves = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        "description(substring:\"Change B\")",
        "-T",
        "change_id.short() ++ \"\\n\"",
    ]);
    let halves: Vec<&str> = halves.lines().collect();
    assert_eq!(halves.len(), 2, "{halves:?}");
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "after split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[1])), "{}", out.text);
    for half in &halves {
        assert!(
            out.text.contains(&half[..8]),
            "should name {half}:\n{}",
            out.text
        );
    }
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_refuses_a_change_with_its_parents_pull_request() {
    // The same, submitting only the upper half: its parent (the lower half)
    // has the same Pull Request.
    let env = TestEnv::new();
    let (_, prs) = stack_with_split_middle(&env);
    let upper = env.change_id(
        "description(substring:\"Change B\") & children(description(substring:\"Change B\"))",
    );
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "-r", &upper, "-m", "after split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[1])), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}
