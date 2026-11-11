//! End-to-end tests of `jj spr sync` against a fake GitHub (see
//! `common/fake_github.rs`). They need `jj` and `git` on `PATH`. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

/// After a sync, `changes` sit in order directly on `main@origin`.
fn assert_rebased_onto_main(env: &TestEnv, changes: &[String]) {
    let main = env.commit_id("main@origin");
    assert_eq!(
        env.parent_of(&changes[0]),
        main,
        "bottom change should be on main@origin"
    );
    for pair in changes.windows(2) {
        assert_eq!(
            env.parent_of(&pair[1]),
            env.commit_id(&pair[0]),
            "{} should sit on {}",
            pair[1],
            pair[0]
        );
    }
}

#[test]
fn sync_after_land() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let land = env.spr(&["land", "-r", &changes[0]]);
    assert!(land.flat().contains("jj spr sync"), "{}", land.text);

    let out = env.spr(&["sync"]);
    assert!(
        !env.is_visible(&changes[0]),
        "landed change should be abandoned"
    );
    assert_rebased_onto_main(&env, &changes[1..]);
    assert!(out.flat().contains("jj spr diff --all"), "{}", out.text);

    env.spr(&["diff", "--all", "-m", "after sync"]);
    let prs = env.check_chain(&changes[1..]);
    assert_eq!(env.pr_files(prs[0]), vec!["b.txt"]);
}

#[test]
fn sync_after_merge_on_github() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[0]);

    env.spr(&["sync"]);
    assert!(!env.is_visible(&changes[0]));
    assert_rebased_onto_main(&env, &changes[1..]);
    env.spr(&["diff", "--all", "-m", "after sync"]);
    env.check_chain(&changes[1..]);
}

#[test]
fn sync_after_merging_part_of_stack() {
    // Merging #2 in GitHub merges the stack below it too.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[1]);
    assert!(env.pr(prs[0]).merged_at.is_some() && env.pr(prs[1]).merged_at.is_some());

    env.spr(&["sync"]);
    assert!(!env.is_visible(&changes[0]) && !env.is_visible(&changes[1]));
    assert_rebased_onto_main(&env, &changes[2..]);
    env.spr(&["diff", "--all", "-m", "after sync"]);
    assert_eq!(env.pr(prs[2]).base_ref, "main");
    assert_eq!(env.pr_files(prs[2]), vec!["c.txt"]);
}

#[test]
fn sync_whole_stack_merged() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[1]);

    let out = env.spr(&["sync"]);
    assert!(changes.iter().all(|c| !env.is_visible(c)));
    assert!(
        out.text.contains("Every change in this stack has merged"),
        "{}",
        out.text
    );
}

#[test]
fn sync_out_of_order_merge() {
    // Cherry-picked PRs all target main and can merge in any order.
    let env = TestEnv::new();
    let changes: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|n| env.commit(&format!("Change {n}"), &format!("{n}.txt"), n))
        .collect();
    env.spr(&["diff", "--all", "--cherry-pick"]);
    let prs = env.prs_for(&changes);
    assert!(prs.iter().all(|&n| env.pr(n).base_ref == "main"));
    env.gh.state().merge_on_github(prs[1]);

    env.spr(&["sync"]);
    assert!(
        !env.is_visible(&changes[1]),
        "merged middle change should be abandoned"
    );
    assert!(env.is_visible(&changes[0]) && env.is_visible(&changes[2]));
    assert_rebased_onto_main(&env, &[changes[0].clone(), changes[2].clone()]);
}

#[test]
fn sync_keeps_unpublished_edits() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    env.spr(&["land", "-r", &changes[0]]);

    // Edit the landed change locally before syncing.
    env.jj(&["edit", &changes[0]]);
    std::fs::write(env.work.join("a.txt"), "a, with more work\n").unwrap();
    env.jj(&["new", &changes[2]]);
    let a_commit = env.commit_id(&changes[0]);
    let b_parent = env.parent_of(&changes[1]);

    let out = env.try_spr(&["sync"]);
    assert!(!out.success, "sync should refuse:\n{}", out.text);
    assert!(
        out.flat().contains("differs from what merged"),
        "{}",
        out.text
    );
    assert!(
        env.is_visible(&changes[0]),
        "the edited change must be kept"
    );
    assert_eq!(
        env.commit_id(&changes[0]),
        a_commit,
        "nothing should change"
    );
    assert_eq!(
        env.parent_of(&changes[1]),
        b_parent,
        "nothing should be rebased"
    );
}

#[test]
fn sync_stops_on_conflict() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    env.spr(&["land", "-r", &changes[0]]);
    // A colleague adds a conflicting b.txt to main.
    env.push_to_main("b.txt", "someone else's b\n");

    let out = env.try_spr(&["sync"]);
    assert!(
        !out.success,
        "sync should report the conflict:\n{}",
        out.text
    );
    assert!(out.flat().contains("left conflicts"), "{}", out.text);
    assert!(out.flat().contains("jj undo"), "{}", out.text);
    let conflicted = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        "conflicts() & main@origin..",
        "-T",
        "change_id ++ \"\\n\"",
    ]);
    assert!(
        conflicted.lines().any(|c| c == changes[1]),
        "B should be conflicted: {conflicted}"
    );
    assert!(
        out.text.contains(&changes[1][..8]),
        "sync should name B:\n{}",
        out.text
    );
}

#[test]
fn sync_nothing_merged() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    env.push_to_main("other.txt", "other\n");
    env.jj(&["git", "fetch"]);
    let before: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();

    let out = env.spr(&["sync"]);
    assert!(
        out.text.contains("Nothing in this stack has merged"),
        "{}",
        out.text
    );
    let after: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();
    assert_eq!(
        before, after,
        "sync must not rebase just because main moved"
    );
}

#[test]
fn sync_leaves_closed_unmerged() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().close_on_github(prs[0]);

    let out = env.spr(&["sync"]);
    assert!(
        out.flat().contains("closed without merging"),
        "{}",
        out.text
    );
    assert!(changes.iter().all(|c| env.is_visible(c)));
}

#[test]
fn sync_dry_run() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    env.spr(&["land", "-r", &changes[0]]);
    let before: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();

    let out = env.spr(&["sync", "--dry-run"]);
    assert!(out.text.contains("Would abandon"), "{}", out.text);
    assert!(out.text.contains("Would rebase"), "{}", out.text);
    let after: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();
    assert_eq!(before, after, "a dry run must not change anything");
}

#[test]
fn sync_legacy_stack() {
    // sync works for stacks that use jj-spr's own base branches too.
    let env = TestEnv::new();
    let changes = vec![
        env.commit("Change A", "a.txt", "a\n"),
        env.commit("Change B", "b.txt", "b\n"),
    ];
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "legacy"]);
    let prs = env.prs_for(&changes);
    env.spr(&["land", "-r", &changes[0]]);

    env.spr(&["sync"]);
    assert!(!env.is_visible(&changes[0]));
    assert_rebased_onto_main(&env, &changes[1..]);
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "after sync"]);
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
}

#[test]
fn sync_after_local_rebase_without_resubmitting() {
    // The local stack was rebased onto a newer main but not resubmitted
    // before the bottom PR merged. Its trees differ from what merged, but it
    // has nothing unpublished, so sync must still abandon it.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.push_to_main("other.txt", "other\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    env.gh.state().merge_on_github(prs[0]);

    env.spr(&["sync"]);
    assert!(
        !env.is_visible(&changes[0]),
        "the merged change should be abandoned"
    );
    assert_rebased_onto_main(&env, &changes[1..]);
}
