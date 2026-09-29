/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr stack`: show a local stack next to its Pull Requests and GitHub's
//! stacks, with their CI checks and reviews, and point out anything that
//! does not match.
//!
//! It is read-only: it does not fetch, push or change any change.

use crate::{
    error::{Error, Result},
    github::{
        CheckState, GitHubStack, PullRequest, PullRequestState, PullRequestStatus, ReviewStatus,
    },
    message::MessageSection,
    output::output,
    stacks::{
        ReviewDecision, ReviewedChange, ReviewedPullRequest, ReviewedState, format_pr_list,
        ready_to_land, review_stack,
    },
};

#[derive(Debug, clap::Parser)]
pub struct StackOptions {
    /// Top of the stack to show. Every change between the main branch and
    /// this revision is included. Defaults to '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Check base branches and GitHub stack membership as for native stacks,
    /// regardless of `spr.nativeStacks`
    #[clap(long)]
    native_stack: bool,

    /// Skip the checks that only apply to native stacks, regardless of
    /// `spr.nativeStacks`
    #[clap(long, conflicts_with = "native_stack")]
    no_native_stack: bool,

    /// Show each Pull Request's checks, reviewers and unresolved review
    /// threads
    #[clap(short = 'v', long)]
    verbose: bool,

    /// Print the stack as JSON, bottom first, instead
    #[clap(long, conflicts_with = "verbose")]
    json: bool,
}

struct Row {
    change_id: String,
    title: String,
    pull_request: Option<PullRequest>,
    /// Checks and threads, for open Pull Requests.
    status: Option<PullRequestStatus>,
    reviewed: ReviewedChange,
}

pub async fn stack(
    opts: StackOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let native = if opts.native_stack {
        true
    } else if opts.no_native_stack {
        false
    } else {
        config.native_stacks
    };
    let trunk = format!("{}@{}", config.master_ref.branch_name(), config.remote_name);
    let target = opts.revision.as_deref().unwrap_or("@-");

    let commits = jj.get_prepared_commits_from_to(config, &trunk, target, false)?;
    if commits.is_empty() {
        if opts.json {
            println!(
                "{}",
                serde_json::json!({"trunk": trunk, "github_stacks": [], "changes": [],
                                   "findings": [], "ready_to_land": []})
            );
        } else {
            output("👋", &format!("No changes between {trunk} and {target}."))?;
        }
        return Ok(());
    }
    jj.require_linear(&commits, "showing the stack")?;
    let change_ids = jj.change_ids_for(&commits)?;

    // Look up every Pull Request at once, and GitHub's stacks alongside.
    let tasks: Vec<_> = commits
        .iter()
        .map(|c| {
            c.pull_request_number
                .map(|n| tokio::spawn(gh.clone().get_pull_request(n)))
        })
        .collect();
    let stacks = gh.list_stacks().await?;
    let mut pull_requests = Vec::new();
    for task in tasks {
        pull_requests.push(match task {
            Some(task) => Some(task.await??),
            None => None,
        });
    }
    let status_tasks: Vec<_> = pull_requests
        .iter()
        .map(|pr| {
            pr.as_ref()
                .filter(|pr| pr.state == PullRequestState::Open)
                .map(|pr| {
                    let gh = gh.clone();
                    let number = pr.number;
                    tokio::spawn(async move { gh.get_pull_request_status(number).await })
                })
        })
        .collect();

    let mut rows = Vec::new();
    for ((commit, pull_request), status_task) in commits.iter().zip(pull_requests).zip(status_tasks)
    {
        let status = match status_task {
            Some(task) => Some(task.await??),
            None => None,
        };
        let change_id = change_ids
            .get(&commit.oid)
            .cloned()
            .ok_or_else(|| Error::new(format!("could not find the change ID of {}", commit.oid)))?;
        let title = commit
            .message
            .get(&MessageSection::Title)
            .cloned()
            .unwrap_or_else(|| "(no title)".to_string());
        let cherry_pick = commit
            .message
            .get(&MessageSection::CherryPick)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        let reviewed_pr = pull_request.as_ref().map(|pr| {
            let checks_in = |state: CheckState| -> Vec<String> {
                status
                    .iter()
                    .flat_map(|s| &s.checks)
                    .filter(|c| c.state == state)
                    .map(|c| c.name.clone())
                    .collect()
            };
            ReviewedPullRequest {
                number: pr.number,
                state: match (pr.state.clone(), pr.merge_commit) {
                    (PullRequestState::Open, _) => ReviewedState::Open,
                    (_, Some(_)) => ReviewedState::Merged,
                    (_, None) => ReviewedState::Closed,
                },
                draft: pr.is_draft,
                base: pr.base.branch_name().to_string(),
                head: pr.head.branch_name().to_string(),
                review: match pr.review_status {
                    Some(ReviewStatus::Approved) => ReviewDecision::Approved,
                    Some(ReviewStatus::Rejected) => ReviewDecision::ChangesRequested,
                    Some(ReviewStatus::Requested) => ReviewDecision::Required,
                    None => ReviewDecision::None,
                },
                failing_checks: checks_in(CheckState::Failed),
                pending_checks: checks_in(CheckState::Pending),
                unresolved_threads: status.as_ref().map_or(0, |s| s.unresolved_threads.len()),
            }
        });
        // An open, non-cherry-picked PR's head has exactly the local change's
        // tree after `jj spr diff`; anything else means it needs submitting.
        // (A head we don't have locally can't be compared, so it isn't flagged.)
        let differs_from_pr = match &pull_request {
            Some(pr) if pr.state == PullRequestState::Open && !cherry_pick => pr
                .github_head_oid
                .and_then(|head| jj.tree_if_present(head))
                .is_some_and(|tree| {
                    jj.get_tree_oid_for_commit(commit.oid)
                        .map(|local| local != tree)
                        .unwrap_or(false)
                }),
            _ => false,
        };

        rows.push(Row {
            reviewed: ReviewedChange {
                label: format!("{} {}", short(&change_id), title),
                pr: reviewed_pr,
                cherry_pick,
                differs_from_pr,
            },
            change_id,
            title,
            pull_request,
            status,
        });
    }

    let reviewed: Vec<ReviewedChange> = rows.iter().map(|r| r.reviewed.clone()).collect();
    let findings = review_stack(
        &reviewed,
        stacks.as_deref(),
        native,
        config.master_ref.branch_name(),
    );
    let ready = ready_to_land(&reviewed, native, config.require_approval);

    if opts.json {
        print_json(&rows, stacks.as_deref(), &findings, ready, &trunk);
        return Ok(());
    }

    print_stacks(&rows, stacks.as_deref())?;
    print_rows(&rows, &trunk, opts.verbose)?;

    println!();
    if findings.is_empty() {
        output("✅", "Everything matches GitHub.")?;
    }
    for finding in findings {
        output("⚠️", &finding.message())?;
    }
    if ready > 0 {
        let numbers: Vec<u64> = rows[..ready]
            .iter()
            .filter_map(|r| r.pull_request.as_ref().map(|pr| pr.number))
            .collect();
        output(
            "🛬",
            &format!(
                "{} {} ready to land: `jj spr land -r {}`",
                format_pr_list(&numbers),
                if ready == 1 { "is" } else { "are" },
                short(&rows[ready - 1].change_id)
            ),
        )?;
    }
    Ok(())
}

fn short(change_id: &str) -> &str {
    &change_id[..change_id.len().min(8)]
}

/// Name the GitHub stacks this stack's Pull Requests belong to.
fn print_stacks(rows: &[Row], stacks: Option<&[GitHubStack]>) -> Result<()> {
    for stack in relevant_stacks(rows, stacks) {
        let members: Vec<u64> = stack.pull_requests.iter().map(|pr| pr.number).collect();
        output(
            "🧱",
            &format!(
                "GitHub stack #{}: {} (bottom first)",
                stack.number,
                format_pr_list(&members)
            ),
        )?;
    }
    Ok(())
}

fn relevant_stacks<'a>(rows: &[Row], stacks: Option<&'a [GitHubStack]>) -> Vec<&'a GitHubStack> {
    let numbers: Vec<u64> = rows
        .iter()
        .filter_map(|r| r.pull_request.as_ref().map(|pr| pr.number))
        .collect();
    stacks
        .unwrap_or_default()
        .iter()
        .filter(|s| numbers.iter().any(|n| s.contains(*n)))
        .collect()
}

fn state_label(reviewed: &ReviewedPullRequest) -> &'static str {
    match reviewed.state {
        ReviewedState::Open => "open",
        ReviewedState::Merged => "merged",
        ReviewedState::Closed => "closed",
    }
}

fn review_label(review: ReviewDecision) -> &'static str {
    match review {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes requested",
        ReviewDecision::Required => "review required",
        ReviewDecision::None => "no review",
    }
}

/// The overall state of a Pull Request's checks, or `None` if it has none.
fn checks_summary(
    reviewed: &ReviewedPullRequest,
    status: &PullRequestStatus,
) -> Option<CheckState> {
    if !reviewed.failing_checks.is_empty() {
        Some(CheckState::Failed)
    } else if !reviewed.pending_checks.is_empty() {
        Some(CheckState::Pending)
    } else if status.checks.is_empty() {
        None
    } else {
        Some(CheckState::Passed)
    }
}

fn check_mark(state: Option<CheckState>) -> &'static str {
    match state {
        Some(CheckState::Passed) => "✓",
        Some(CheckState::Failed) => "✗",
        Some(CheckState::Pending) => "●",
        None => "-",
    }
}

/// Green for passed, red for failed, yellow for running, dim for none.
/// `console` leaves the text plain when colors are off (not a terminal, or
/// `NO_COLOR` set).
fn check_style(state: Option<CheckState>) -> console::Style {
    let style = console::Style::new();
    match state {
        Some(CheckState::Passed) => style.green(),
        Some(CheckState::Failed) => style.red(),
        Some(CheckState::Pending) => style.yellow(),
        None => style.dim(),
    }
}

fn review_cell(reviewed: &ReviewedPullRequest) -> String {
    let mut cell = review_label(reviewed.review).to_string();
    match reviewed.unresolved_threads {
        0 => {}
        1 => cell.push_str(", 1 thread"),
        n => cell.push_str(&format!(", {n} threads")),
    }
    cell
}

/// The stack top first, like `jj log`.
fn print_rows(rows: &[Row], trunk: &str, verbose: bool) -> Result<()> {
    const TITLE_WIDTH: usize = 40;
    let title_width = rows
        .iter()
        .map(|r| r.title.chars().count().min(TITLE_WIDTH))
        .max()
        .unwrap_or(0);
    let pr_width = rows
        .iter()
        .map(|r| {
            r.pull_request
                .as_ref()
                .map_or(1, |pr| pr.number.to_string().len() + 1)
        })
        .max()
        .unwrap_or(1);

    // The columns after the title for each row: PR, state, and for open
    // PRs, checks and review; then the base branch.
    struct Cells {
        pr: String,
        state: String,
        checks: String,
        checks_style: console::Style,
        review: String,
        base: Option<String>,
    }
    let cells: Vec<Cells> = rows
        .iter()
        .map(|row| match (&row.pull_request, &row.reviewed.pr) {
            (Some(pull_request), Some(reviewed)) => {
                let mut state = state_label(reviewed).to_string();
                if reviewed.draft {
                    state.push_str(", draft");
                }
                if row.reviewed.differs_from_pr {
                    state.push_str(", needs update");
                }
                let (checks, checks_style, review) = match &row.status {
                    Some(status) => {
                        let summary = checks_summary(reviewed, status);
                        (
                            format!("CI {}", check_mark(summary)),
                            check_style(summary),
                            review_cell(reviewed),
                        )
                    }
                    None => (String::new(), console::Style::new(), String::new()),
                };
                Cells {
                    pr: format!("#{}", pull_request.number),
                    state,
                    checks,
                    checks_style,
                    review,
                    base: Some(reviewed.base.clone()),
                }
            }
            _ => Cells {
                pr: "-".to_string(),
                state: "not submitted".to_string(),
                checks: String::new(),
                checks_style: console::Style::new(),
                review: String::new(),
                base: None,
            },
        })
        .collect();
    let width = |f: fn(&Cells) -> usize| {
        cells
            .iter()
            .filter(|c| c.base.is_some())
            .map(f)
            .max()
            .unwrap_or(0)
    };
    let state_width = width(|c| c.state.chars().count());
    let checks_width = width(|c| c.checks.chars().count());
    let review_width = width(|c| c.review.chars().count());

    let term = console::Term::stdout();
    for (row, cell) in rows.iter().zip(&cells).rev() {
        let title: String = if row.title.chars().count() > TITLE_WIDTH {
            let cut: String = row.title.chars().take(TITLE_WIDTH - 1).collect();
            format!("{cut}…")
        } else {
            row.title.clone()
        };
        let detail = match &cell.base {
            Some(base) => {
                let mut detail = pad(&cell.state, state_width);
                // With `-v` the lines under the row show checks and review.
                if checks_width > 0 && !verbose {
                    detail.push_str("  ");
                    // Pad outside the colour codes, which have no width.
                    detail.push_str(&cell.checks_style.apply_to(&cell.checks).to_string());
                    detail.push_str(&pad("", checks_width - cell.checks.chars().count()));
                    detail.push_str("  ");
                    detail.push_str(&pad(&cell.review, review_width));
                }
                format!("{detail}  → {base}")
            }
            None => cell.state.clone(),
        };
        term.write_line(
            format!(
                "○  {}  {}  {:<pr_width$}  {}",
                console::style(short(&row.change_id)).magenta(),
                pad(&title, title_width),
                cell.pr,
                detail,
            )
            .trim_end(),
        )?;
        if verbose {
            for line in detail_lines(row) {
                term.write_line(&format!("│    {line}"))?;
            }
        }
    }
    term.write_line(&format!("◆  {trunk}"))?;
    Ok(())
}

/// Pad to a width in characters (`format!` would count bytes of `✓`).
fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(len)))
}

/// The `--verbose` lines under an open Pull Request's row.
fn detail_lines(row: &Row) -> Vec<String> {
    let (Some(pr), Some(reviewed), Some(status)) =
        (&row.pull_request, &row.reviewed.pr, &row.status)
    else {
        return vec![];
    };
    let mut lines = Vec::new();

    let checks = if status.checks.is_empty() {
        "none".to_string()
    } else {
        status
            .checks
            .iter()
            .map(|c| {
                let state = Some(c.state);
                check_style(state)
                    .apply_to(format!("{} {}", check_mark(state), c.name))
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("   ")
    };
    lines.push(format!("checks   {checks}"));

    let by = |wanted: ReviewStatus| {
        let mut logins: Vec<String> = pr
            .reviewers
            .iter()
            .filter(|(_, status)| **status == wanted)
            .map(|(login, _)| format!("@{login}"))
            .collect();
        logins.sort();
        logins.join(", ")
    };
    let mut parts = Vec::new();
    for (wanted, label) in [
        (ReviewStatus::Approved, "approved by"),
        (ReviewStatus::Rejected, "changes requested by"),
    ] {
        let logins = by(wanted);
        if !logins.is_empty() {
            parts.push(format!("{label} {logins}"));
        }
    }
    if parts.is_empty() {
        parts.push(review_label(reviewed.review).to_string());
    }
    // The Reviewers section also names those who have reviewed already.
    let waiting: Vec<String> = pr
        .sections
        .get(&MessageSection::Reviewers)
        .into_iter()
        .flat_map(|r| r.split(','))
        .map(str::trim)
        .filter(|r| !r.is_empty() && !pr.reviewers.contains_key(*r))
        .map(|r| {
            if r.starts_with('#') {
                r.to_string()
            } else {
                format!("@{r}")
            }
        })
        .collect();
    if !waiting.is_empty() {
        parts.push(format!("requested: {}", waiting.join(", ")));
    }
    let review = parts.join(" · ");
    lines.push(format!("review   {review}"));

    for (i, thread) in status.unresolved_threads.iter().enumerate() {
        let location = match thread.line {
            Some(line) => format!("{}:{line}", thread.path),
            None => thread.path.clone(),
        };
        let author = thread
            .author
            .as_ref()
            .map(|a| format!("  @{a}"))
            .unwrap_or_default();
        let first_line = thread.body.lines().next().unwrap_or_default();
        let excerpt: String = if first_line.chars().count() > 50 {
            format!("{}…", first_line.chars().take(49).collect::<String>())
        } else {
            first_line.to_string()
        };
        let label = if i == 0 { "threads " } else { "        " };
        lines.push(format!("{label} ▸ {location}{author}  \"{excerpt}\""));
    }
    lines
}

fn print_json(
    rows: &[Row],
    stacks: Option<&[GitHubStack]>,
    findings: &[crate::stacks::Finding],
    ready: usize,
    trunk: &str,
) {
    use serde_json::{Value, json};

    let changes: Vec<Value> = rows
        .iter()
        .map(|row| {
            let pull_request = match (&row.pull_request, &row.reviewed.pr) {
                (Some(pr), Some(reviewed)) => {
                    let status = row.status.as_ref().map(|status| {
                        let checks_state = match checks_summary(reviewed, status) {
                            Some(CheckState::Failed) => "failure",
                            Some(CheckState::Pending) => "pending",
                            Some(CheckState::Passed) => "success",
                            None => "none",
                        };
                        json!({
                            "checks": {
                                "state": checks_state,
                                "failing": reviewed.failing_checks,
                                "pending": reviewed.pending_checks,
                            },
                            "unresolved_threads": status.unresolved_threads.iter().map(|t| json!({
                                "path": t.path, "line": t.line,
                                "author": t.author, "body": t.body,
                            })).collect::<Vec<_>>(),
                        })
                    });
                    let mut approved_by: Vec<&String> = pr
                        .reviewers
                        .iter()
                        .filter(|(_, s)| **s == ReviewStatus::Approved)
                        .map(|(login, _)| login)
                        .collect();
                    approved_by.sort();
                    json!({
                        "number": pr.number,
                        "state": state_label(reviewed),
                        "draft": reviewed.draft,
                        "base": reviewed.base,
                        "head": reviewed.head,
                        "needs_update": row.reviewed.differs_from_pr,
                        "review": {
                            "decision": match reviewed.review {
                                ReviewDecision::Approved => "approved",
                                ReviewDecision::ChangesRequested => "changes_requested",
                                ReviewDecision::Required => "review_required",
                                ReviewDecision::None => "none",
                            },
                            "approved_by": approved_by,
                        },
                        "status": status,
                    })
                }
                _ => Value::Null,
            };
            json!({
                "change_id": row.change_id,
                "title": row.title,
                "pull_request": pull_request,
            })
        })
        .collect();

    let github_stacks: Vec<Value> = relevant_stacks(rows, stacks)
        .into_iter()
        .map(|stack| {
            json!({
                "number": stack.number,
                "pull_requests": stack.pull_requests.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            })
        })
        .collect();
    let findings: Vec<Value> = findings
        .iter()
        .map(
            |f| json!({"kind": f.kind(), "pull_request": f.pull_request(), "message": f.message()}),
        )
        .collect();
    let ready_to_land: Vec<u64> = rows[..ready]
        .iter()
        .filter_map(|r| r.pull_request.as_ref().map(|pr| pr.number))
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "trunk": trunk,
            "github_stacks": github_stacks,
            "changes": changes,
            "findings": findings,
            "ready_to_land": ready_to_land,
        }))
        .expect("JSON serialises")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_colors() {
        let styled = |state| {
            check_style(state)
                .force_styling(true)
                .apply_to(check_mark(state))
                .to_string()
        };
        assert_eq!(styled(Some(CheckState::Passed)), "\u{1b}[32m✓\u{1b}[0m");
        assert_eq!(styled(Some(CheckState::Failed)), "\u{1b}[31m✗\u{1b}[0m");
        assert_eq!(styled(Some(CheckState::Pending)), "\u{1b}[33m●\u{1b}[0m");
        assert_eq!(styled(None), "\u{1b}[2m-\u{1b}[0m");
    }
}
