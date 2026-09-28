//! A jj repository whose `origin` is a bare Git repository served by the fake
//! GitHub, plus helpers for driving `jj` and `jj-spr` and checking the result.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use super::fake_github::{FakeGitHub, PullRequest};

pub struct TestEnv {
    // Declared first so the server stops before the directory is removed.
    pub gh: FakeGitHub,
    pub root: TempDir,
    pub work: PathBuf,
    pub seed: PathBuf,
    env: Vec<(String, String)>,
}

/// Output of a command: whether it succeeded, and stdout plus stderr.
pub struct Outcome {
    pub success: bool,
    pub text: String,
}

impl Outcome {
    /// The output with jj-spr's line wrapping undone, for substring checks.
    pub fn flat(&self) -> String {
        self.text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

fn jj_spr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_jj-spr")
}

impl TestEnv {
    pub fn new() -> Self {
        Self::with_stacks(true)
    }

    pub fn with_stacks(stacks_enabled: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("spr-native-")
            .tempdir()
            .expect("temp dir");
        let remote = root.path().join("remote.git");
        let work = root.path().join("work");
        let seed = root.path().join("seed");
        std::fs::write(root.path().join("jj-config.toml"), "").unwrap();
        std::fs::write(root.path().join("gitconfig"), "").unwrap();

        let env: Vec<(String, String)> = [
            (
                "JJ_CONFIG",
                root.path().join("jj-config.toml").display().to_string(),
            ),
            ("JJ_USER", "Test User".into()),
            ("JJ_EMAIL", "test@example.com".into()),
            ("GIT_AUTHOR_NAME", "Test User".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test User".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
            (
                "GIT_CONFIG_GLOBAL",
                root.path().join("gitconfig").display().to_string(),
            ),
            ("NO_COLOR", "1".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let run = |cwd: &Path, program: &str, args: &[&str]| {
            let outcome = run_command(cwd, &env, program, args);
            assert!(
                outcome.success,
                "{program} {args:?} failed:\n{}",
                outcome.text
            );
        };
        let remote_arg = remote.display().to_string();
        run(
            root.path(),
            "git",
            &["init", "-q", "--bare", "-b", "main", &remote_arg],
        );
        run(
            root.path(),
            "git",
            &["clone", "-q", &remote_arg, &seed.display().to_string()],
        );
        std::fs::write(seed.join("README"), "hello\n").unwrap();
        run(&seed, "git", &["add", "README"]);
        run(&seed, "git", &["commit", "-q", "-m", "Initial commit"]);
        run(&seed, "git", &["push", "-q", "origin", "main"]);

        let gh = FakeGitHub::start("fake", "repo", &remote, stacks_enabled);

        run(
            root.path(),
            "jj",
            &[
                "git",
                "clone",
                "--colocate",
                &remote_arg,
                &work.display().to_string(),
            ],
        );
        for (key, value) in [
            ("spr.githubRepository", "fake/repo"),
            ("spr.branchPrefix", "spr/"),
            ("spr.githubAuthToken", "fake-token"),
            ("spr.githubApiUrl", gh.url.as_str()),
            ("spr.nativeStacks", "true"),
        ] {
            run(&work, "jj", &["config", "set", "--repo", key, value]);
        }

        Self {
            gh,
            root,
            work,
            seed,
            env,
        }
    }

    // -- commands ----------------------------------------------------------

    pub fn jj(&self, args: &[&str]) -> String {
        let outcome = run_command(&self.work, &self.env, "jj", args);
        assert!(outcome.success, "jj {args:?} failed:\n{}", outcome.text);
        outcome.text
    }

    /// Run jj for its output only (without notices it prints on stderr).
    pub fn jj_query(&self, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(&self.work)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Run jj-spr and require it to succeed.
    pub fn spr(&self, args: &[&str]) -> Outcome {
        let outcome = self.try_spr(args);
        assert!(outcome.success, "jj-spr {args:?} failed:\n{}", outcome.text);
        outcome
    }

    /// Run jj-spr and return its outcome whether or not it succeeded.
    pub fn try_spr(&self, args: &[&str]) -> Outcome {
        run_command(&self.work, &self.env, jj_spr_bin(), args)
    }

    pub fn git_in(&self, cwd: &Path, args: &[&str]) {
        let outcome = run_command(cwd, &self.env, "git", args);
        assert!(outcome.success, "git {args:?} failed:\n{}", outcome.text);
    }

    /// Create a change on top of `@-`; returns its change ID.
    pub fn commit(&self, title: &str, path: &str, content: &str) -> String {
        std::fs::write(self.work.join(path), content).unwrap();
        self.jj(&["commit", "-m", title]);
        self.change_id("@-")
    }

    pub fn change_id(&self, rev: &str) -> String {
        self.jj_query(&["log", "--no-graph", "-r", rev, "-T", "change_id"])
            .trim()
            .to_string()
    }

    pub fn commit_id(&self, rev: &str) -> String {
        self.jj_query(&["log", "--no-graph", "-r", rev, "-T", "commit_id"])
            .trim()
            .to_string()
    }

    /// Build a stack of changes `a`, `b`, … and submit it with `diff --all`.
    pub fn build_stack(&self, names: &[&str]) -> Vec<String> {
        let changes = names
            .iter()
            .map(|n| {
                self.commit(
                    &format!("Change {}", n.to_uppercase()),
                    &format!("{n}.txt"),
                    &format!("{n}\n"),
                )
            })
            .collect();
        self.spr(&["diff", "--all", "-m", "initial"]);
        changes
    }

    // -- observations ------------------------------------------------------

    pub fn pr_for(&self, change: &str) -> u64 {
        let description = self.jj_query(&["log", "--no-graph", "-r", change, "-T", "description"]);
        description
            .lines()
            .find_map(|line| line.strip_prefix("Pull Request:"))
            .and_then(|url| url.trim().rsplit('/').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| {
                panic!("change {change} has no Pull Request trailer:\n{description}")
            })
    }

    pub fn prs_for(&self, changes: &[String]) -> Vec<u64> {
        changes.iter().map(|c| self.pr_for(c)).collect()
    }

    pub fn pr(&self, number: u64) -> PullRequest {
        self.gh.state().prs[&number].clone()
    }

    pub fn branch(&self, name: &str) -> Option<String> {
        self.gh.state().branches().get(name).cloned()
    }

    pub fn head_of(&self, number: u64) -> String {
        let head_ref = self.pr(number).head_ref;
        self.branch(&head_ref)
            .unwrap_or_else(|| panic!("PR #{number}'s branch {head_ref} is missing"))
    }

    pub fn remote_git(&self, args: &[&str]) -> String {
        self.gh.state().git(args)
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self.gh.state().is_ancestor(ancestor, descendant)
    }

    /// The files a change touches, sorted.
    pub fn files_in(&self, change: &str) -> Vec<String> {
        let mut files: Vec<String> = self
            .jj_query(&["diff", "--name-only", "-r", change])
            .split_whitespace()
            .map(str::to_string)
            .collect();
        files.sort();
        files
    }

    /// Files in GitHub's diff for a PR: changes on head since its merge base
    /// with base.
    pub fn pr_files(&self, number: u64) -> Vec<String> {
        let state = self.gh.state();
        let pr = &state.prs[&number];
        let heads = state.branches();
        let (base, head) = (&heads[&pr.base_ref], &heads[&pr.head_ref]);
        let merge_base = state.git(&["merge-base", base, head]);
        let mut files: Vec<String> = state
            .git(&["diff", "--name-only", &merge_base, head])
            .split_whitespace()
            .map(str::to_string)
            .collect();
        files.sort();
        files
    }

    pub fn stacks(&self) -> BTreeMap<u64, Vec<u64>> {
        self.gh.state().stacks.clone()
    }

    pub fn active_stacks(&self) -> Vec<Vec<u64>> {
        let state = self.gh.state();
        state
            .stacks
            .values()
            .map(|members| state.active(members))
            .filter(|active| !active.is_empty())
            .collect()
    }

    pub fn request_count(&self) -> usize {
        self.gh.state().requests.len()
    }

    /// Recorded requests since `from` with this method whose path contains
    /// `fragment`.
    pub fn requests_since(&self, from: usize, method: &str, fragment: &str) -> Vec<String> {
        self.gh.state().requests[from..]
            .iter()
            .filter(|r| r.method == method && r.path.contains(fragment))
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }

    /// Whether a change is still visible (not abandoned).
    pub fn is_visible(&self, change: &str) -> bool {
        !self
            .jj_query(&[
                "log",
                "--no-graph",
                "-r",
                &format!("present({change})"),
                "-T",
                "change_id",
            ])
            .trim()
            .is_empty()
    }

    /// The commit ID of a change's parent.
    pub fn parent_of(&self, change: &str) -> String {
        self.commit_id(&format!("{change}-"))
    }

    /// Commit a new file on main from another clone and push it, as a
    /// colleague would.
    pub fn push_to_main(&self, path: &str, content: &str) {
        self.git_in(&self.seed, &["pull", "-q", "origin", "main"]);
        std::fs::write(self.seed.join(path), content).unwrap();
        self.git_in(&self.seed, &["add", path]);
        self.git_in(&self.seed, &["commit", "-q", "-m", &format!("Add {path}")]);
        self.git_in(&self.seed, &["push", "-q", "origin", "main"]);
    }

    /// Commit a file on a Pull Request's branch from another clone and push
    /// it, as a colleague would. Returns the branch's new head.
    pub fn push_to_pr_branch(&self, number: u64, path: &str, content: &str) -> String {
        let branch = self.pr(number).head_ref;
        self.git_in(&self.seed, &["fetch", "-q", "origin", &branch]);
        self.git_in(
            &self.seed,
            &["checkout", "-q", "-B", "colleague", "FETCH_HEAD"],
        );
        std::fs::write(self.seed.join(path), content).unwrap();
        self.git_in(&self.seed, &["add", path]);
        self.git_in(
            &self.seed,
            &["commit", "-q", "-m", &format!("Change {path}")],
        );
        self.git_in(
            &self.seed,
            &["push", "-q", "origin", &format!("HEAD:{branch}")],
        );
        self.git_in(&self.seed, &["checkout", "-q", "main"]);
        self.head_of(number)
    }

    /// Every PR targets the one below, shows only its own files, has the local
    /// change's tree, and the PRs form one active stack.
    pub fn check_chain(&self, changes: &[String]) -> Vec<u64> {
        let prs = self.prs_for(changes);
        for (i, &number) in prs.iter().enumerate() {
            let pr = self.pr(number);
            let expected_base = if i == 0 {
                "main".to_string()
            } else {
                self.pr(prs[i - 1]).head_ref
            };
            assert!(
                pr.is_open(),
                "PR #{number} is {} ({:?})",
                pr.state,
                pr.closed_reason
            );
            assert_eq!(pr.base_ref, expected_base, "PR #{number}'s base");

            let own = self.files_in(&changes[i]);
            assert_eq!(self.pr_files(number), own, "files in PR #{number}'s diff");

            let head_tree =
                self.remote_git(&["rev-parse", &format!("{}^{{tree}}", self.head_of(number))]);
            let local_tree = run_command(
                &self.work,
                &self.env,
                "git",
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", self.commit_id(&changes[i])),
                ],
            );
            assert_eq!(
                head_tree,
                local_tree.text.trim(),
                "PR #{number}'s tree vs local change"
            );
        }
        assert!(
            self.active_stacks().contains(&prs),
            "expected stack {prs:?}, have {:?}",
            self.stacks()
        );
        prs
    }
}

fn run_command(cwd: &Path, env: &[(String, String)], program: &str, args: &[&str]) -> Outcome {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("could not run {program}: {e}"));
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if std::env::var_os("SPR_TEST_VERBOSE").is_some() {
        eprintln!("$ {program} {}\n{text}", args.join(" "));
    }
    Outcome {
        success: output.status.success(),
        text,
    }
}
