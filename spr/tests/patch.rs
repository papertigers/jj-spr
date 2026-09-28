//! End-to-end tests of `jj spr patch` against a fake GitHub (see
//! `common/fake_github.rs`). They need `jj` and `git` on `PATH`. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

/// Submit a stack, then forget the local changes, as if the stack were
/// someone else's. Returns the stack's Pull Request numbers.
fn someone_elses_stack(env: &TestEnv, names: &[&str]) -> Vec<u64> {
    let changes = env.build_stack(names);
    let prs = env.prs_for(&changes);
    let mut abandon = vec!["abandon"];
    abandon.extend(changes.iter().map(String::as_str));
    env.jj(&abandon);
    prs
}

/// The changes linked to `prs`, found by their `Pull Request:` trailers.
fn changes_for(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter()
        .map(|n| {
            let revset = format!(
                "mutable() & description(substring:\"https://github.com/fake/repo/pull/{n}\\n\")"
            );
            let found = env.jj_query(&[
                "log",
                "--no-graph",
                "-r",
                &revset,
                "-T",
                "change_id ++ \"\\n\"",
            ]);
            let found: Vec<&str> = found.lines().collect();
            assert_eq!(
                found.len(),
                1,
                "expected one change for #{n}, found {found:?}"
            );
            found[0].to_string()
        })
        .collect()
}

#[test]
fn patch_fetches_whole_stack() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b", "c"]);

    // Asking for the middle PR fetches the whole stack.
    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert!(out.text.contains("part of a stack"), "{}", out.text);
    let changes = changes_for(&env, &prs);

    assert_eq!(env.parent_of(&changes[1]), env.commit_id(&changes[0]));
    assert_eq!(env.parent_of(&changes[2]), env.commit_id(&changes[1]));
    for (change, name) in changes.iter().zip(["a", "b", "c"]) {
        assert_eq!(env.files_in(change), vec![format!("{name}.txt")]);
    }
    let title = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        &changes[1],
        "-T",
        "description.first_line()",
    ]);
    assert_eq!(title.trim(), "Change B");
    assert_eq!(
        env.parent_of("@"),
        env.commit_id(&changes[2]),
        "@ should be on top"
    );

    // The changes match their Pull Requests, so resubmitting pushes nothing.
    let heads: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();
    env.spr(&["diff", "--all", "-m", "after patch"]);
    let after: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();
    assert_eq!(heads, after, "no PR branch should move");
    env.check_chain(&changes);

    // And editing one updates the existing PR.
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    env.spr(&["diff", "--all", "-m", "edit B"]);
    assert_eq!(env.check_chain(&changes), prs);
}

#[test]
fn patch_pull_request_out_of_date_with_its_base() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);

    // Someone pushes to the bottom PR's branch; the PR above is not updated.
    env.push_to_pr_branch(prs[0], "extra.txt", "extra\n");

    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    assert_eq!(env.files_in(&changes[0]), vec!["a.txt", "extra.txt"]);
    assert_eq!(
        env.files_in(&changes[1]),
        vec!["b.txt"],
        "B should hold only its own diff, not a revert of extra.txt"
    );
}

#[test]
fn patch_single_pull_request() {
    let env = TestEnv::with_stacks(false);
    let change = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &change, "-m", "initial"]);
    let pr = env.pr_for(&change);
    env.jj(&["abandon", &change]);

    env.spr(&["patch", &pr.to_string()]);
    let changes = changes_for(&env, &[pr]);
    assert_eq!(env.files_in(&changes[0]), vec!["a.txt"]);
    assert_eq!(env.parent_of(&changes[0]), env.commit_id("main@origin"));
}

#[test]
fn patch_no_stack_and_no_checkout() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    let before = env.commit_id("@");

    env.spr(&["patch", "--no-stack", "--no-checkout", &prs[0].to_string()]);
    let changes = changes_for(&env, &prs[..1]);
    assert_eq!(env.files_in(&changes[0]), vec!["a.txt"]);
    assert_eq!(env.commit_id("@"), before, "@ must not move");
    let linked = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        "mutable() & description(substring:\"Pull Request:\")",
        "-T",
        "change_id ++ \"\\n\"",
    ]);
    assert_eq!(
        linked.lines().count(),
        1,
        "only #{} should be fetched",
        prs[0]
    );
}

/// The ID of jj's latest operation.
fn last_op(env: &TestEnv) -> String {
    env.jj_query(&["op", "log", "--no-graph", "-n1", "-T", "id"])
        .trim()
        .to_string()
}

/// Descriptions of the jj operations since `op`.
fn ops_since(env: &TestEnv, op: &str) -> Vec<String> {
    env.jj_query(&[
        "op",
        "log",
        "--no-graph",
        "-T",
        "id ++ \" \" ++ description ++ \"\\n\"",
    ])
    .lines()
    .take_while(|l| !l.starts_with(op))
    .map(str::to_string)
    .collect()
}

fn assert_only_fetched(env: &TestEnv, op: &str) {
    let since = ops_since(env, op);
    assert!(
        since
            .iter()
            .all(|l| l.contains("fetch") || l.contains("import")),
        "patch should only have fetched: {since:?}"
    );
}

#[test]
fn patch_own_stack_is_up_to_date() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let op = last_op(&env);

    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert_eq!(out.text.matches("is up to date").count(), 2, "{}", out.text);
    assert_eq!(
        changes_for(&env, &prs),
        changes,
        "no change should be added"
    );
    assert_only_fetched(&env, &op);
}

#[test]
fn patch_updates_from_colleagues_push() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b", "c"]);
    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    let a_before = env.commit_id(&changes[0]);

    // The colleague pushes to B only; C on GitHub is now behind it.
    env.push_to_pr_branch(prs[1], "b-fix.txt", "fix\n");

    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert!(
        out.text
            .contains(&format!("#{}: {} is up to date", prs[0], &changes[0][..8])),
        "{}",
        out.text
    );
    assert!(
        out.text
            .contains(&format!("#{}: updated {}", prs[1], &changes[1][..8])),
        "{}",
        out.text
    );
    assert!(
        out.text
            .contains(&format!("#{}: {} is up to date", prs[2], &changes[2][..8])),
        "{}",
        out.text
    );
    assert_eq!(
        changes_for(&env, &prs),
        changes,
        "the same changes are updated"
    );
    assert_eq!(
        env.commit_id(&changes[0]),
        a_before,
        "A must not be rewritten"
    );
    assert_eq!(env.files_in(&changes[1]), vec!["b-fix.txt", "b.txt"]);
    assert_eq!(env.files_in(&changes[2]), vec!["c.txt"]);
    assert_eq!(env.parent_of(&changes[2]), env.commit_id(&changes[1]));

    // Now diff has nothing to overwrite; it brings C up to date with B.
    env.spr(&["diff", "--all", "-m", "after update"]);
    env.check_chain(&changes);
}

#[test]
fn patch_keeps_unpushed_local_edits() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    let b_before = env.commit_id(&changes[1]);

    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert!(
        out.flat()
            .contains("has local edits that are not on GitHub yet"),
        "{}",
        out.text
    );
    assert_eq!(
        env.commit_id(&changes[1]),
        b_before,
        "B must be kept as it is"
    );
}

#[test]
fn patch_stops_when_both_sides_changed() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    env.push_to_pr_branch(prs[1], "b-fix.txt", "fix\n");
    let op = last_op(&env);

    let out = env.try_spr(&["patch", &prs[1].to_string()]);
    assert!(!out.success, "patch should refuse:\n{}", out.text);
    assert!(
        out.flat().contains("changed both locally and on GitHub"),
        "{}",
        out.text
    );
    assert!(out.text.contains(&changes[1][..8]), "{}", out.text);
    assert_only_fetched(&env, &op);
}

#[test]
fn patch_adds_missing_changes_on_top() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b", "c"]);
    env.spr(&["patch", "--no-stack", &prs[0].to_string()]);
    let a = changes_for(&env, &prs[..1]);

    let out = env.spr(&["patch", &prs[0].to_string()]);
    assert!(out.text.contains("is up to date"), "{}", out.text);
    let changes = changes_for(&env, &prs);
    assert_eq!(changes[0], a[0]);
    assert_eq!(env.parent_of(&changes[1]), env.commit_id(&changes[0]));
    assert_eq!(env.files_in(&changes[2]), vec!["c.txt"]);
    assert_eq!(
        env.parent_of("@"),
        env.commit_id(&changes[2]),
        "@ should be on top"
    );
}

#[test]
fn patch_refuses_missing_change_below_existing_one() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    env.spr(&["patch", "--no-stack", &prs[1].to_string()]);
    let op = last_op(&env);

    let out = env.try_spr(&["patch", &prs[1].to_string()]);
    assert!(!out.success, "patch should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains(&format!("#{} has no local change", prs[0])),
        "{}",
        out.text
    );
    assert_only_fetched(&env, &op);
}

#[test]
fn patch_refuses_closed_pull_request() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a"]);
    env.gh.state().close_on_github(prs[0]);

    let out = env.try_spr(&["patch", &prs[0].to_string()]);
    assert!(!out.success, "patch should refuse:\n{}", out.text);
    assert!(out.flat().contains("is not open"), "{}", out.text);
}

#[test]
fn patch_updates_when_colleague_edits_the_same_lines() {
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    env.push_to_pr_branch(prs[1], "b.txt", "b, fixed by a colleague\n");

    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert!(
        out.text.contains(&format!("#{}: updated", prs[1])),
        "{}",
        out.text
    );
    let b = env.jj_query(&["file", "show", "-r", &changes[1], "b.txt"]);
    assert_eq!(b, "b, fixed by a colleague\n");
}

#[test]
fn patch_updates_after_local_rebase() {
    // Rebasing the stack locally onto a newer main leaves nothing unpushed,
    // even though no tree matches GitHub's any more.
    let env = TestEnv::new();
    let prs = someone_elses_stack(&env, &["a", "b"]);
    env.spr(&["patch", &prs[1].to_string()]);
    let changes = changes_for(&env, &prs);
    env.push_to_main("other.txt", "other\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    env.push_to_pr_branch(prs[1], "b-fix.txt", "fix\n");

    let out = env.spr(&["patch", &prs[1].to_string()]);
    assert!(
        out.text
            .contains(&format!("#{}: {} is up to date", prs[0], &changes[0][..8])),
        "{}",
        out.text
    );
    assert!(
        out.text.contains(&format!("#{}: updated", prs[1])),
        "{}",
        out.text
    );
    assert_eq!(env.files_in(&changes[1]), vec!["b-fix.txt", "b.txt"]);
    assert_eq!(
        env.parent_of(&changes[0]),
        env.commit_id("main@origin"),
        "the rebase is kept"
    );
}
