//! A fake GitHub API for testing jj-spr's native stack support.
//!
//! This is a Rust port of `tests/native_stacks/fake_github.py`; keep the two
//! in step. It serves the REST and GraphQL calls jj-spr makes, backed by a bare
//! Git repository that jj-spr pushes to, and models GitHub's stacked pull
//! requests.
//!
//! The stack behaviour follows the fake GitHub in jj-stack
//! (<https://github.com/bos/jj-stack>, `tests/support/fake_github.py`,
//! Apache-2.0), whose authors checked it against the real API:
//!
//! - a stack needs two or more pull requests, each PR's base must be the head
//!   branch of the PR below it, and a PR can be in only one stack
//! - the base of a stacked PR cannot be changed with PATCH (422)
//! - stacked PRs are merged with `PUT .../merge-async` and polled with
//!   `GET .../merge-async/{uuid}`; merging a PR merges the stack prefix below
//!   it, and GitHub then rebases the remaining members onto the new base
//! - merged members stay listed in the stack as history
//!
//! Behaviour that is an assumption of this fake rather than observed:
//!
//! - the ordinary merge endpoint refuses stacked PRs
//! - the post-merge rebase replays each surviving PR's net diff as one commit
//!
//! Everything else (closing PRs when their base or head branch is deleted,
//! squash merges as a three-way merge) is ordinary GitHub behaviour.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use serde_json::{Value, json};

const GIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Fake GitHub"),
    ("GIT_AUTHOR_EMAIL", "fake-github@example.com"),
    ("GIT_COMMITTER_NAME", "Fake GitHub"),
    ("GIT_COMMITTER_EMAIL", "fake-github@example.com"),
];

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub state: String,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub last_head_sha: String,
    pub closed_reason: Option<String>,
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Debug, Clone)]
struct MergeOperation {
    uuid: String,
    pr_number: u64,
    method: String,
    expected_head: String,
    status: String,
    message: String,
    sha: Option<String>,
}

/// One recorded API request: method, path and JSON body.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// State of the fake repository.
#[derive(Debug)]
pub struct State {
    pub owner: String,
    pub name: String,
    pub git_dir: PathBuf,
    pub stacks_enabled: bool,
    pub prs: BTreeMap<u64, PullRequest>,
    pub stacks: BTreeMap<u64, Vec<u64>>,
    merge_ops: HashMap<u64, MergeOperation>,
    pub requests: Vec<Recorded>,
    next_number: u64,
    next_stack: u64,
}

struct ApiError {
    status: u16,
    message: String,
}

fn api_error<T>(status: u16, message: impl Into<String>) -> Result<T, ApiError> {
    Err(ApiError {
        status,
        message: message.into(),
    })
}

type Reply = Result<(u16, Option<Value>), ApiError>;

fn now() -> String {
    // A fixed-format timestamp is all the clients look at.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Days since the Unix epoch to a (year, month, day) civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

impl State {
    // -- git ---------------------------------------------------------------

    pub fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(args)
            .envs(GIT_IDENTITY)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run git");
        assert!(
            output.status.success(),
            "fake github: git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub fn branches(&self) -> BTreeMap<String, String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["show-ref", "--heads"])
            .output()
            .expect("failed to run git");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (sha, name) = line.split_once(' ')?;
                Some((
                    name.strip_prefix("refs/heads/")?.to_string(),
                    sha.to_string(),
                ))
            })
            .collect()
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // -- pull request state ------------------------------------------------

    /// Apply what GitHub does on its own when branches change.
    fn refresh(&mut self) {
        let heads = self.branches();
        for pr in self.prs.values_mut() {
            if !pr.is_open() {
                continue;
            }
            if let Some(sha) = heads.get(&pr.head_ref) {
                pr.last_head_sha = sha.clone();
            }
            if !heads.contains_key(&pr.base_ref) {
                pr.state = "closed".into();
                pr.closed_reason = Some(format!("base branch {} was deleted", pr.base_ref));
            } else if !heads.contains_key(&pr.head_ref) {
                pr.state = "closed".into();
                pr.closed_reason = Some(format!("head branch {} was deleted", pr.head_ref));
            }
        }
    }

    pub fn stack_of(&self, number: u64) -> Option<u64> {
        self.stacks
            .iter()
            .find(|(_, members)| members.contains(&number))
            .map(|(n, _)| *n)
    }

    pub fn active(&self, members: &[u64]) -> Vec<u64> {
        members
            .iter()
            .copied()
            .filter(|n| self.prs[n].is_open())
            .collect()
    }

    fn pr_mut(&mut self, number: u64) -> Result<&mut PullRequest, ApiError> {
        match self.prs.get_mut(&number) {
            Some(pr) => Ok(pr),
            None => api_error(404, "Not Found"),
        }
    }

    // -- payloads ----------------------------------------------------------

    fn web(&self, path: &str) -> String {
        format!("https://github.com/{}/{}{}", self.owner, self.name, path)
    }

    fn api(&self, path: &str) -> String {
        format!(
            "https://api.github.test/repos/{}/{}{}",
            self.owner, self.name, path
        )
    }

    fn user_payload() -> Value {
        let base = "https://api.github.test/users/fake";
        json!({
            "login": "fake", "id": 1, "node_id": "U_1",
            "avatar_url": "https://avatars.github.test/u/1", "gravatar_id": "",
            "url": base, "html_url": "https://github.com/fake",
            "followers_url": format!("{base}/followers"),
            "following_url": format!("{base}/following"),
            "gists_url": format!("{base}/gists"), "starred_url": format!("{base}/starred"),
            "subscriptions_url": format!("{base}/subscriptions"),
            "organizations_url": format!("{base}/orgs"), "repos_url": format!("{base}/repos"),
            "events_url": format!("{base}/events"),
            "received_events_url": format!("{base}/received_events"),
            "type": "User", "site_admin": false,
        })
    }

    fn pr_payload(&self, number: u64) -> Value {
        let pr = &self.prs[&number];
        let heads = self.branches();
        let n = pr.number;
        let head_sha = heads.get(&pr.head_ref).unwrap_or(&pr.last_head_sha).clone();
        json!({
            "url": self.api(&format!("/pulls/{n}")), "id": 1000 + n, "node_id": format!("PR_{n}"),
            "html_url": self.web(&format!("/pull/{n}")),
            "diff_url": self.web(&format!("/pull/{n}.diff")),
            "patch_url": self.web(&format!("/pull/{n}.patch")),
            "issue_url": self.api(&format!("/issues/{n}")),
            "commits_url": self.api(&format!("/pulls/{n}/commits")),
            "review_comments_url": self.api(&format!("/pulls/{n}/comments")),
            "review_comment_url": self.api("/pulls/comments{/number}"),
            "comments_url": self.api(&format!("/issues/{n}/comments")),
            "statuses_url": self.api(&format!("/statuses/{head_sha}")),
            "number": n, "state": pr.state, "locked": false, "title": pr.title,
            "user": Self::user_payload(), "body": pr.body, "labels": [],
            "created_at": "2026-01-01T00:00:00Z", "updated_at": now(),
            "closed_at": if pr.is_open() { Value::Null } else { json!(now()) },
            "merged": pr.merged_at.is_some(), "merged_at": pr.merged_at,
            "merge_commit_sha": pr.merge_commit_sha,
            "assignees": [], "requested_reviewers": [], "requested_teams": [],
            "head": {"label": format!("{}:{}", self.owner, pr.head_ref), "ref": pr.head_ref,
                     "sha": head_sha},
            "base": {"label": format!("{}:{}", self.owner, pr.base_ref), "ref": pr.base_ref,
                     "sha": heads.get(&pr.base_ref).cloned().unwrap_or_default()},
            "_links": {}, "author_association": "OWNER", "draft": pr.draft,
            "additions": 0, "deletions": 0, "changed_files": 0, "commits": 1,
            "review_comments": 0, "comments": 0,
        })
    }

    fn stack_payload(&self, number: u64) -> Value {
        let heads = self.branches();
        let members: Vec<Value> = self.stacks[&number]
            .iter()
            .map(|n| {
                let pr = &self.prs[n];
                json!({
                    "number": n,
                    "state": pr.state,
                    "merged_at": pr.merged_at,
                    "head": {"ref": pr.head_ref,
                             "sha": heads.get(&pr.head_ref).unwrap_or(&pr.last_head_sha)},
                })
            })
            .collect();
        json!({"number": number, "pull_requests": members})
    }

    fn graphql_pr(&self, number: u64) -> Value {
        let pr = &self.prs[&number];
        let state = if pr.merged_at.is_some() {
            "MERGED".to_string()
        } else {
            pr.state.to_uppercase()
        };
        json!({
            "number": pr.number,
            "state": state,
            "reviewDecision": null,
            "isDraft": pr.draft,
            "title": pr.title,
            "body": pr.body,
            "baseRefName": pr.base_ref,
            "headRefName": pr.head_ref,
            "headRefOid": self.branches().get(&pr.head_ref).unwrap_or(&pr.last_head_sha),
            "mergeable": "MERGEABLE",
            "mergeCommit": pr.merge_commit_sha.as_ref().map(|oid| json!({"oid": oid})),
            "latestOpinionatedReviews": {"nodes": []},
            "reviewRequests": {"nodes": []},
        })
    }

    // -- stacks ------------------------------------------------------------

    fn validate_stack(
        &self,
        admitted: &[u64],
        chained: &[u64],
        allowed_stack: Option<u64>,
    ) -> Result<(), ApiError> {
        let mut unique = chained.to_vec();
        unique.sort();
        unique.dedup();
        if unique.len() != chained.len() {
            return api_error(422, "Duplicate pull request.");
        }
        for n in admitted {
            match self.prs.get(n) {
                None => return api_error(422, "Pull request does not exist."),
                Some(pr) if !pr.is_open() => {
                    return api_error(422, "Pull request is not admissible.");
                }
                _ => {}
            }
        }
        for pair in chained.windows(2) {
            if self.prs[&pair[1]].base_ref != self.prs[&pair[0]].head_ref {
                return api_error(422, "Pull request bases do not form a chain.");
            }
        }
        for (number, members) in &self.stacks {
            if Some(*number) != allowed_stack && members.iter().any(|m| admitted.contains(m)) {
                return api_error(422, "Pull request already belongs to a stack.");
            }
        }
        Ok(())
    }

    fn create_stack(&mut self, members: Vec<u64>) -> Result<Value, ApiError> {
        if members.len() < 2 {
            return api_error(422, "A stack requires two pull requests.");
        }
        self.validate_stack(&members, &members, None)?;
        let number = self.next_stack;
        self.next_stack += 1;
        self.stacks.insert(number, members);
        Ok(self.stack_payload(number))
    }

    fn add_to_stack(&mut self, number: u64, added: Vec<u64>) -> Result<Value, ApiError> {
        let Some(existing) = self.stacks.get(&number).cloned() else {
            return api_error(404, "Not Found");
        };
        if added.is_empty() {
            return api_error(422, "No pull requests to append.");
        }
        let mut chained = self.active(&existing);
        chained.extend(&added);
        self.validate_stack(&added, &chained, Some(number))?;
        let mut members = existing;
        members.extend(added);
        self.stacks.insert(number, members);
        Ok(self.stack_payload(number))
    }

    fn unstack(&mut self, number: u64) -> Result<Option<Value>, ApiError> {
        let Some(members) = self.stacks.get(&number).cloned() else {
            return api_error(404, "Not Found");
        };
        let retained: Vec<u64> = members
            .into_iter()
            .filter(|n| self.prs[n].merged_at.is_some())
            .collect();
        if retained.is_empty() {
            self.stacks.remove(&number);
            Ok(None)
        } else {
            self.stacks.insert(number, retained);
            Ok(Some(self.stack_payload(number)))
        }
    }

    // -- merging -----------------------------------------------------------

    fn squash_merge(&mut self, number: u64) -> String {
        let heads = self.branches();
        let (base_ref, head_ref, title) = {
            let pr = &self.prs[&number];
            (pr.base_ref.clone(), pr.head_ref.clone(), pr.title.clone())
        };
        let (base, head) = (heads[&base_ref].clone(), heads[&head_ref].clone());
        let tree = self.git(&["merge-tree", "--write-tree", &base, &head]);
        let tree = tree.lines().next().unwrap().to_string();
        let message = format!("{title} (#{number})");
        let commit = self.git(&["commit-tree", &tree, "-p", &base, "-m", &message]);
        self.git(&["update-ref", &format!("refs/heads/{base_ref}"), &commit]);
        let pr = self.prs.get_mut(&number).unwrap();
        pr.merged_at = Some(now());
        pr.merge_commit_sha = Some(commit.clone());
        pr.last_head_sha = head;
        pr.state = "closed".into();
        commit
    }

    /// GitHub rebasing a surviving stack member after a merge below it.
    fn rebase_onto(&mut self, number: u64, old_base: &str, new_base_ref: &str) {
        let heads = self.branches();
        let (head_ref, title) = {
            let pr = &self.prs[&number];
            (pr.head_ref.clone(), pr.title.clone())
        };
        let new_base = heads[new_base_ref].clone();
        let head = heads[&head_ref].clone();
        let merge_base = format!("--merge-base={old_base}");
        let tree = self.git(&["merge-tree", "--write-tree", &merge_base, &new_base, &head]);
        let tree = tree.lines().next().unwrap().to_string();
        let commit = self.git(&["commit-tree", &tree, "-p", &new_base, "-m", &title]);
        self.git(&["update-ref", &format!("refs/heads/{head_ref}"), &commit]);
        let pr = self.prs.get_mut(&number).unwrap();
        pr.base_ref = new_base_ref.to_string();
        pr.last_head_sha = commit;
    }

    fn complete_stack_merge(&mut self, pr_number: u64) {
        let members = match self.stack_of(pr_number) {
            Some(stack) => self.active(&self.stacks[&stack].clone()),
            None => vec![pr_number],
        };
        let cut = members.iter().position(|n| *n == pr_number).unwrap() + 1;
        let (prefix, survivors) = (members[..cut].to_vec(), members[cut..].to_vec());
        let base_ref = self.prs[&prefix[0]].base_ref.clone();
        let old_top = self.branches()[&self.prs[&prefix[cut - 1]].head_ref].clone();
        for number in &prefix {
            self.prs.get_mut(number).unwrap().base_ref = base_ref.clone();
            self.squash_merge(*number);
        }
        let (mut previous_base, mut previous_old) = (base_ref.clone(), old_top);
        for number in survivors {
            let head_ref = self.prs[&number].head_ref.clone();
            let old_head = self.branches()[&head_ref].clone();
            self.rebase_onto(number, &previous_old, &previous_base);
            previous_base = head_ref;
            previous_old = old_head;
        }
        let sha = self.branches()[&base_ref].clone();
        let op = self.merge_ops.get_mut(&pr_number).unwrap();
        op.status = "merged".into();
        op.message = "Pull request successfully merged.".into();
        op.sha = Some(sha);
    }

    fn merge_payload(op: &MergeOperation) -> Value {
        json!({
            "status": op.status,
            "details": {
                "expected_head_sha": op.expected_head,
                "merge_action": "direct_merge",
                "merge_method": op.method,
                "message": op.message,
                "sha": op.sha,
                "uuid": op.uuid,
            },
        })
    }

    // -- routing -----------------------------------------------------------

    fn route(&mut self, method: &str, path: &str, query: &str, body: &Value) -> Reply {
        if path == "/graphql" && method == "POST" {
            return Ok((200, Some(self.graphql(body))));
        }
        let prefix = format!("/repos/{}/{}", self.owner, self.name);
        let Some(rest) = path.strip_prefix(&prefix) else {
            return api_error(404, "Not Found");
        };
        let rest = rest.to_string();
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

        if segments.first() == Some(&"stacks") {
            if !self.stacks_enabled {
                return api_error(404, "Not Found");
            }
            return self.route_stacks(method, &segments, query, body);
        }

        match (method, segments.as_slice()) {
            ("POST", ["pulls"]) => {
                let heads = self.branches();
                for key in ["head", "base"] {
                    let branch = body[key].as_str().unwrap_or_default();
                    if !heads.contains_key(branch) {
                        return api_error(422, format!("Branch {branch:?} does not exist."));
                    }
                }
                let number = self.next_number;
                self.next_number += 1;
                let head_ref = body["head"].as_str().unwrap().to_string();
                self.prs.insert(
                    number,
                    PullRequest {
                        number,
                        title: body["title"].as_str().unwrap_or_default().to_string(),
                        body: body["body"].as_str().unwrap_or_default().to_string(),
                        last_head_sha: heads[&head_ref].clone(),
                        head_ref,
                        base_ref: body["base"].as_str().unwrap().to_string(),
                        draft: body["draft"].as_bool().unwrap_or(false),
                        state: "open".into(),
                        merged_at: None,
                        merge_commit_sha: None,
                        closed_reason: None,
                    },
                );
                Ok((201, Some(self.pr_payload(number))))
            }
            ("GET", ["pulls", n]) => {
                let n = parse_number(n)?;
                self.pr_mut(n)?;
                Ok((200, Some(self.pr_payload(n))))
            }
            ("PATCH", ["pulls", n]) => {
                let n = parse_number(n)?;
                self.update_pr(n, body)?;
                Ok((200, Some(self.pr_payload(n))))
            }
            ("POST", ["pulls", n, "requested_reviewers"]) => {
                let n = parse_number(n)?;
                self.pr_mut(n)?;
                Ok((201, Some(self.pr_payload(n))))
            }
            ("PUT", ["pulls", n, "merge"]) => {
                let n = parse_number(n)?;
                let head_ref = self.pr_mut(n)?.head_ref.clone();
                if self.stack_of(n).is_some() {
                    return api_error(405, "Stacked pull requests must be merged as a stack.");
                }
                if let Some(sha) = body["sha"].as_str()
                    && self.branches().get(&head_ref).map(String::as_str) != Some(sha)
                {
                    return api_error(409, "Head branch was modified.");
                }
                let sha = self.squash_merge(n);
                Ok((
                    200,
                    Some(json!({"sha": sha, "merged": true,
                                "message": "Pull Request successfully merged"})),
                ))
            }
            ("PUT", ["pulls", n, "merge-async"]) => {
                let n = parse_number(n)?;
                let (head_ref, draft) = {
                    let pr = self.pr_mut(n)?;
                    (pr.head_ref.clone(), pr.draft)
                };
                if body["merge_action"].as_str() != Some("direct_merge") {
                    return api_error(400, "Merge action does not match policy.");
                }
                let expected = body["sha"].as_str().unwrap_or_default().to_string();
                if self.branches().get(&head_ref) != Some(&expected) {
                    return api_error(400, "Target head changed.");
                }
                let active = match self.stack_of(n) {
                    Some(stack) => self.active(&self.stacks[&stack].clone()),
                    None => vec![n],
                };
                if !active.contains(&n) || draft {
                    return api_error(400, "Target is not mergeable.");
                }
                let op = MergeOperation {
                    uuid: format!("merge-{}", self.merge_ops.len() + 1),
                    pr_number: n,
                    method: body["merge_method"].as_str().unwrap_or("merge").to_string(),
                    expected_head: expected,
                    status: "pending".into(),
                    message: "Merge request enqueued.".into(),
                    sha: None,
                };
                let payload = Self::merge_payload(&op);
                self.merge_ops.insert(n, op);
                Ok((202, Some(payload)))
            }
            ("GET", ["pulls", n, "merge-async", uuid]) => {
                let n = parse_number(n)?;
                let pending = match self.merge_ops.get(&n) {
                    Some(op) if op.uuid == *uuid => op.status == "pending",
                    _ => return api_error(404, "Not Found"),
                };
                if pending {
                    let pr_number = self.merge_ops[&n].pr_number;
                    self.complete_stack_merge(pr_number);
                }
                Ok((200, Some(Self::merge_payload(&self.merge_ops[&n]))))
            }
            _ => api_error(404, "Not Found"),
        }
    }

    fn update_pr(&mut self, number: u64, body: &Value) -> Result<(), ApiError> {
        let (base_ref, state) = {
            let pr = self.pr_mut(number)?;
            (pr.base_ref.clone(), pr.state.clone())
        };
        if let Some(new_base) = body["base"].as_str()
            && new_base != base_ref
        {
            if self.stack_of(number).is_some() {
                return api_error(
                    422,
                    "A stacked pull request's base cannot be updated directly.",
                );
            }
            if state != "open" {
                return api_error(
                    422,
                    "Cannot change the base branch of a closed pull request.",
                );
            }
            if !self.branches().contains_key(new_base) {
                return api_error(422, format!("Branch {new_base:?} does not exist."));
            }
            self.pr_mut(number)?.base_ref = new_base.to_string();
        }
        let pr = self.pr_mut(number)?;
        if let Some(title) = body["title"].as_str() {
            pr.title = title.to_string();
        }
        if body.get("body").is_some() {
            pr.body = body["body"].as_str().unwrap_or_default().to_string();
        }
        if body["state"].as_str() == Some("closed") && pr.is_open() {
            pr.state = "closed".into();
            pr.closed_reason = Some("closed by request".into());
        }
        Ok(())
    }

    fn route_stacks(
        &mut self,
        method: &str,
        segments: &[&str],
        query: &str,
        body: &Value,
    ) -> Reply {
        let int_list = |body: &Value| -> Vec<u64> {
            body["pull_requests"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default()
        };
        match (method, segments) {
            ("GET", ["stacks"]) => {
                let params = parse_query(query);
                let page: u64 = params.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let wanted: Option<u64> = params.get("pull_request").and_then(|p| p.parse().ok());
                let numbers: Vec<u64> = if page == 1 {
                    self.stacks
                        .iter()
                        .filter(|(_, m)| wanted.is_none_or(|w| m.contains(&w)))
                        .map(|(n, _)| *n)
                        .collect()
                } else {
                    vec![]
                };
                let list: Vec<Value> = numbers.iter().map(|n| self.stack_payload(*n)).collect();
                Ok((200, Some(Value::Array(list))))
            }
            ("POST", ["stacks"]) => Ok((201, Some(self.create_stack(int_list(body))?))),
            ("GET", ["stacks", n]) => {
                let n = parse_number(n)?;
                if !self.stacks.contains_key(&n) {
                    return api_error(404, "Not Found");
                }
                Ok((200, Some(self.stack_payload(n))))
            }
            ("POST", ["stacks", n, "add"]) => {
                let n = parse_number(n)?;
                Ok((200, Some(self.add_to_stack(n, int_list(body))?)))
            }
            ("POST", ["stacks", n, "unstack"]) => {
                let n = parse_number(n)?;
                Ok(match self.unstack(n)? {
                    None => (204, None),
                    Some(remaining) => (200, Some(remaining)),
                })
            }
            _ => api_error(404, "Not Found"),
        }
    }

    fn graphql(&self, body: &Value) -> Value {
        let name = body["operationName"].as_str().unwrap_or_default();
        let variables = &body["variables"];
        match name {
            "PullRequestQuery" | "PullRequestMergeabilityQuery" => {
                let data = variables["number"]
                    .as_u64()
                    .filter(|n| self.prs.contains_key(n))
                    .map(|n| self.graphql_pr(n));
                json!({"data": {"repository": {"pullRequest": data}}})
            }
            "OpenPullRequestBranchesQuery" => {
                let nodes: Vec<Value> = self
                    .prs
                    .values()
                    .filter(|pr| pr.is_open())
                    .map(|pr| {
                        json!({"number": pr.number, "headRefName": pr.head_ref,
                               "baseRefName": pr.base_ref})
                    })
                    .collect();
                json!({"data": {"repository": {"pullRequests": {
                    "nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}})
            }
            "SearchQuery" => {
                let nodes: Vec<Value> = self
                    .prs
                    .values()
                    .filter(|pr| pr.is_open())
                    .map(|pr| {
                        json!({"__typename": "PullRequest", "number": pr.number,
                               "title": pr.title, "url": self.web(&format!("/pull/{}", pr.number)),
                               "reviewDecision": null})
                    })
                    .collect();
                json!({"data": {"search": {"nodes": nodes}}})
            }
            other => {
                json!({"errors": [{"message": format!("fake github: unknown operation {other:?}")}]})
            }
        }
    }
}

fn parse_number(text: &str) -> Result<u64, ApiError> {
    text.parse().or_else(|_| api_error(404, "Not Found"))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A running fake GitHub. The server stops when this is dropped.
pub struct FakeGitHub {
    state: Arc<Mutex<State>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    pub url: String,
}

impl FakeGitHub {
    pub fn start(owner: &str, name: &str, git_dir: &Path, stacks_enabled: bool) -> Self {
        let state = Arc::new(Mutex::new(State {
            owner: owner.into(),
            name: name.into(),
            git_dir: git_dir.to_path_buf(),
            stacks_enabled,
            prs: BTreeMap::new(),
            stacks: BTreeMap::new(),
            merge_ops: HashMap::new(),
            requests: Vec::new(),
            next_number: 1,
            next_stack: 1,
        }));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake github");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();

        let thread = {
            let state = state.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(listener).expect("tokio listener");
                    let app = axum::Router::new().fallback(handle).with_state(state);
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .expect("fake github server");
                });
            })
        };

        Self {
            state,
            shutdown: Some(shutdown),
            thread: Some(thread),
            url,
        }
    }

    /// Lock the fake's state for inspection (or for arranging a scenario).
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Every request goes through `State::route`, mirroring the Python fake.
async fn handle(
    axum::extract::State(state): axum::extract::State<Arc<Mutex<State>>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let method = method.as_str().to_string();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    // The fake shells out to git, so do the work off the async runtime.
    let (status, payload) = tokio::task::spawn_blocking(move || {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });
        state.refresh();
        let reply = state.route(&method, &path, &query, &body);
        state.refresh();
        match reply {
            Ok(reply) => reply,
            Err(error) => (
                error.status,
                Some(json!({"message": error.message, "status": error.status.to_string()})),
            ),
        }
    })
    .await
    .expect("fake github handler panicked");

    let status = StatusCode::from_u16(status).expect("valid status");
    let body = payload.map(|p| p.to_string()).unwrap_or_default();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}
