//! End-to-end tests of `jj spr stack` against a fake GitHub (see
//! `common/fake_github.rs`). They need `jj` and `git` on `PATH`. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

/// The line of `jj spr stack` output that shows `change`.
fn line_for<'a>(text: &'a str, change: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(&change[..8]))
        .unwrap_or_else(|| panic!("no line for {change}:\n{text}"))
}

#[test]
fn stack_clean() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let out = env.spr(&["stack"]);

    // Stacks are numbered like pull requests, so this one is #4.
    let stack = *env.stacks().keys().next().unwrap();
    assert!(
        out.text
            .contains(&format!("GitHub stack #{stack}: #1, #2, #3")),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("Everything matches GitHub"),
        "{}",
        out.text
    );
    assert!(line_for(&out.text, &changes[0]).contains("#1"));
    assert!(line_for(&out.text, &changes[0]).contains("→ main"));
    assert!(line_for(&out.text, &changes[1]).contains("→ spr/change-a"));
    assert!(line_for(&out.text, &changes[2]).contains("→ spr/change-b"));
    // Top of the stack first, like jj log.
    let position = |c: &String| out.text.find(&c[..8]).unwrap();
    assert!(position(&changes[2]) < position(&changes[1]));
    assert!(position(&changes[1]) < position(&changes[0]));
    assert!(out.text.contains("◆  main@origin"), "{}", out.text);
}

#[test]
fn stack_is_read_only() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let heads = env.gh.state().branches();
    let commits: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();
    let before = env.request_count();

    env.spr(&["stack"]);
    for method in ["POST", "PATCH", "PUT"] {
        let writes = env.requests_since(before, method, "/repos/");
        assert!(writes.is_empty(), "stack changed GitHub: {writes:?}");
    }
    assert_eq!(env.gh.state().branches(), heads);
    let after: Vec<String> = changes.iter().map(|c| env.commit_id(c)).collect();
    assert_eq!(commits, after, "stack changed local commits");
}

#[test]
fn stack_reports_unsubmitted_and_out_of_date() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    // Edit B after submitting it, and add C without submitting.
    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("b.txt"), "b, revised\n").unwrap();
    env.jj(&["new", &changes[1]]);
    let c = env.commit("Change C", "c.txt", "c\n");

    let out = env.spr(&["stack"]);
    assert!(
        line_for(&out.text, &changes[1]).contains("needs update"),
        "{}",
        out.text
    );
    assert!(
        !line_for(&out.text, &changes[0]).contains("needs update"),
        "{}",
        out.text
    );
    assert!(
        line_for(&out.text, &c).contains("not submitted"),
        "{}",
        out.text
    );
    let flat = out.flat();
    assert!(
        flat.contains("The local change for #2 differs"),
        "{}",
        out.text
    );
    assert!(flat.contains("has no Pull Request yet"), "{}", out.text);
    assert!(!flat.contains("Everything matches"), "{}", out.text);

    // Submitting fixes both.
    env.spr(&["diff", "--all", "-m", "update"]);
    assert!(
        env.spr(&["stack"])
            .text
            .contains("Everything matches GitHub")
    );
}

#[test]
fn stack_reports_drafts() {
    let env = TestEnv::new();
    let changes: Vec<String> = ["a", "b"]
        .iter()
        .map(|n| env.commit(&format!("Change {n}"), &format!("{n}.txt"), n))
        .collect();
    env.spr(&["diff", "--all", "--draft"]);
    let out = env.spr(&["stack"]);
    assert!(
        line_for(&out.text, &changes[0]).contains("draft"),
        "{}",
        out.text
    );
    assert!(out.flat().contains("#1 is a draft"), "{}", out.text);
}

#[test]
fn stack_after_merge_suggests_sync() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    env.spr(&["land", "-r", &changes[0]]);

    let out = env.spr(&["stack"]);
    assert!(
        line_for(&out.text, &changes[0]).contains("merged"),
        "{}",
        out.text
    );
    assert!(
        out.flat().contains("#1 has merged. Run `jj spr sync`"),
        "{}",
        out.text
    );

    env.spr(&["sync"]);
    let out = env.spr(&["stack"]);
    assert!(
        out.text.contains("Everything matches GitHub"),
        "{}",
        out.text
    );
}

#[test]
fn stack_reports_unregistered_stack() {
    let env = TestEnv::new();
    env.build_stack(&["a", "b", "c"]);
    env.gh.state().stacks.clear();
    let out = env.spr(&["stack"]);
    assert!(
        out.flat()
            .contains("#1, #2, #3 target each other but are not registered"),
        "{}",
        out.text
    );
}

#[test]
fn stack_reports_missing_member() {
    let env = TestEnv::new();
    env.build_stack(&["a", "b", "c"]);
    let stack = *env.stacks().keys().next().unwrap();
    env.gh.state().stacks.insert(stack, vec![1, 2]);
    let out = env.spr(&["stack"]);
    assert!(
        out.flat()
            .contains(&format!("#3 is not in GitHub stack #{stack} yet")),
        "{}",
        out.text
    );
}

#[test]
fn stack_reports_insertion() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let inserted = env.change_id("@");
    env.jj(&["new", &changes[2]]);
    env.spr(&["diff", "-r", &inserted]);

    let out = env.spr(&["stack"]);
    let flat = out.flat();
    // C still targets B, but X now sits between them.
    assert!(
        flat.contains(&format!(
            "#3 targets spr/change-b, but the change below it is #{} (spr/change-x)",
            env.pr_for(&inserted)
        )),
        "{}",
        out.text
    );
    assert!(flat.contains("jj spr diff --all"), "{}", out.text);
}

#[test]
fn stack_stacks_disabled() {
    let env = TestEnv::with_stacks(false);
    env.build_stack(&["a", "b"]);
    let out = env.spr(&["stack"]);
    assert!(
        out.flat().contains("Stacked pull requests are not enabled"),
        "{}",
        out.text
    );
}

#[test]
fn stack_legacy_base_branches() {
    let env = TestEnv::new();
    let changes = [
        env.commit("Change A", "a.txt", "a\n"),
        env.commit("Change B", "b.txt", "b\n"),
    ];
    env.spr(&["diff", "--all", "--no-native-stack", "-m", "legacy"]);

    let out = env.spr(&["stack", "--no-native-stack"]);
    assert!(
        out.text.contains("Everything matches GitHub"),
        "{}",
        out.text
    );
    assert!(
        line_for(&out.text, &changes[1]).contains("→ spr/main."),
        "{}",
        out.text
    );

    // With native checks, the base branch is reported.
    let out = env.spr(&["stack"]);
    assert!(out.flat().contains("#2 targets spr/main."), "{}", out.text);
}

/// A stack of three with passing, running and failing checks, one approval
/// and one unresolved thread on the top PR.
fn stack_with_status(env: &TestEnv) -> (Vec<String>, Vec<u64>) {
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let mut gh = env.gh.state();
    gh.set_checks(prs[0], &[("build", "success"), ("clippy", "success")]);
    gh.set_checks(prs[1], &[("build", "pending"), ("clippy", "success")]);
    gh.set_checks(prs[2], &[("build", "failure"), ("clippy", "success")]);
    gh.review(prs[0], "alice", "APPROVED");
    gh.review(prs[2], "bob", "CHANGES_REQUESTED");
    gh.add_thread(prs[2], "c.txt", 1, "bob", "Please rename this\nand more");
    drop(gh);
    (changes, prs)
}

#[test]
fn stack_shows_checks_and_reviews() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_status(&env);
    let out = env.spr(&["stack"]);

    assert!(
        line_for(&out.text, &changes[0]).contains("CI ✓  approved"),
        "{}",
        out.text
    );
    assert!(
        line_for(&out.text, &changes[1]).contains("CI ●  no review"),
        "{}",
        out.text
    );
    assert!(
        line_for(&out.text, &changes[2]).contains("CI ✗  changes requested, 1 thread"),
        "{}",
        out.text
    );
    let text = out.flat();
    assert!(
        text.contains(&format!("#{} has failing checks: build.", prs[2])),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!("#{} has changes requested.", prs[2])),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!("#{} has 1 unresolved review thread.", prs[2])),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!(
            "#{} is ready to land: `jj spr land -r {}`",
            prs[0],
            &changes[0][..8]
        )),
        "{}",
        out.text
    );
}

#[test]
fn stack_ready_to_land_several() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let mut gh = env.gh.state();
    gh.set_checks(prs[0], &[("build", "success")]);
    gh.set_checks(prs[1], &[("build", "success")]);
    gh.set_checks(prs[2], &[("build", "failure")]);
    drop(gh);

    let out = env.spr(&["stack"]);
    assert!(
        out.flat().contains(&format!(
            "#{}, #{} are ready to land: `jj spr land -r {}`",
            prs[0],
            prs[1],
            &changes[1][..8]
        )),
        "{}",
        out.text
    );

    // Approval counts once it is required.
    env.jj(&["config", "set", "--repo", "spr.requireApproval", "true"]);
    let out = env.spr(&["stack"]);
    assert!(!out.text.contains("ready to land"), "{}", out.text);
}

#[test]
fn stack_checks_belong_to_the_head() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a"]);
    let prs = env.prs_for(&changes);
    env.gh.state().set_checks(prs[0], &[("build", "success")]);
    assert!(line_for(&env.spr(&["stack"]).text, &changes[0]).contains("CI ✓"));

    std::fs::write(env.work.join("a.txt"), "a, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[0]]);
    env.spr(&["diff", "--all", "-m", "edit"]);
    let out = env.spr(&["stack"]);
    assert!(
        line_for(&out.text, &changes[0]).contains("CI -"),
        "{}",
        out.text
    );
}

#[test]
fn stack_verbose_details() {
    let env = TestEnv::new();
    let (changes, _) = stack_with_status(&env);
    let out = env.spr(&["stack", "-v"]);
    for change in &changes {
        let line = line_for(&out.text, change);
        assert!(
            !line.contains("CI ") && !line.contains("approved") && !line.contains("review"),
            "checks and review belong under the row with -v: {line}"
        );
        assert!(line.contains("→ "), "{line}");
    }
    for expected in [
        "│    checks   ✓ build   ✓ clippy",
        "│    checks   ● build   ✓ clippy",
        "│    checks   ✗ build   ✓ clippy",
        "│    review   approved by @alice\n",
        "│    review   changes requested by @bob\n",
        "│    review   no review\n",
        "│    threads  ▸ c.txt:1  @bob  \"Please rename this\"",
    ] {
        assert!(
            out.text.contains(expected),
            "missing {expected:?}:\n{}",
            out.text
        );
    }
}

#[test]
fn stack_json() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_status(&env);
    let out = env.spr(&["stack", "--json"]);
    let json: serde_json::Value =
        serde_json::from_str(&out.text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{}", out.text));

    assert_eq!(json["trunk"], "main@origin");
    assert_eq!(
        json["github_stacks"][0]["pull_requests"],
        serde_json::json!(prs)
    );
    let entries = json["changes"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["change_id"], changes[0], "bottom first");
    let bottom = &entries[0]["pull_request"];
    assert_eq!(bottom["number"], prs[0]);
    assert_eq!(bottom["review"]["decision"], "approved");
    assert_eq!(
        bottom["review"]["approved_by"],
        serde_json::json!(["alice"])
    );
    assert_eq!(bottom["status"]["checks"]["state"], "success");
    let top = &entries[2]["pull_request"];
    assert_eq!(
        top["status"]["checks"]["failing"],
        serde_json::json!(["build"])
    );
    assert_eq!(top["status"]["unresolved_threads"][0]["path"], "c.txt");
    let kinds: Vec<&str> = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["failing_checks", "changes_requested", "unresolved_threads"]
    );
    assert_eq!(json["ready_to_land"], serde_json::json!([prs[0]]));
}
