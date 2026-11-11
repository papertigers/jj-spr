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
