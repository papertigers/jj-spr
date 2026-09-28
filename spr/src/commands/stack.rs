/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr stack`: show a local stack next to its Pull Requests and GitHub's
//! stacks, and point out anything that does not match.
//!
//! It is read-only: it does not fetch, push or change any change.

use crate::{
    error::{Error, Result},
    github::{GitHubStack, PullRequest, PullRequestState},
    message::MessageSection,
    output::output,
    stacks::{ReviewedChange, ReviewedPullRequest, ReviewedState, format_pr_list, review_stack},
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
}

struct Row {
    change_id: String,
    title: String,
    pull_request: Option<PullRequest>,
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
        output("👋", &format!("No changes between {trunk} and {target}."))?;
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

    let mut rows = Vec::new();
    for (commit, task) in commits.iter().zip(tasks) {
        let pull_request = match task {
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

        let reviewed_pr = pull_request.as_ref().map(|pr| ReviewedPullRequest {
            number: pr.number,
            state: match (pr.state.clone(), pr.merge_commit) {
                (PullRequestState::Open, _) => ReviewedState::Open,
                (_, Some(_)) => ReviewedState::Merged,
                (_, None) => ReviewedState::Closed,
            },
            draft: pr.is_draft,
            base: pr.base.branch_name().to_string(),
            head: pr.head.branch_name().to_string(),
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
        });
    }

    print_stacks(&rows, stacks.as_deref())?;
    print_rows(&rows, &trunk)?;

    let reviewed: Vec<ReviewedChange> = rows.iter().map(|r| r.reviewed.clone()).collect();
    let findings = review_stack(
        &reviewed,
        stacks.as_deref(),
        native,
        config.master_ref.branch_name(),
    );
    println!();
    if findings.is_empty() {
        output("✅", "Everything matches GitHub.")?;
    }
    for finding in findings {
        output("⚠️", &finding.message())?;
    }
    Ok(())
}

fn short(change_id: &str) -> &str {
    &change_id[..change_id.len().min(8)]
}

/// Name the GitHub stacks this stack's Pull Requests belong to.
fn print_stacks(rows: &[Row], stacks: Option<&[GitHubStack]>) -> Result<()> {
    let Some(stacks) = stacks else {
        return Ok(());
    };
    let numbers: Vec<u64> = rows
        .iter()
        .filter_map(|r| r.pull_request.as_ref().map(|pr| pr.number))
        .collect();
    for stack in stacks
        .iter()
        .filter(|s| numbers.iter().any(|n| s.contains(*n)))
    {
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

/// The stack top first, like `jj log`.
fn print_rows(rows: &[Row], trunk: &str) -> Result<()> {
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

    // (PR, state, base) for each row, so the state column can be sized.
    let cells: Vec<(String, String, Option<String>)> = rows
        .iter()
        .map(|row| match (&row.pull_request, &row.reviewed.pr) {
            (Some(pull_request), Some(reviewed)) => {
                let mut state = match reviewed.state {
                    ReviewedState::Open => "open".to_string(),
                    ReviewedState::Merged => "merged".to_string(),
                    ReviewedState::Closed => "closed".to_string(),
                };
                if reviewed.draft {
                    state.push_str(", draft");
                }
                if row.reviewed.differs_from_pr {
                    state.push_str(", needs update");
                }
                (
                    format!("#{}", pull_request.number),
                    state,
                    Some(reviewed.base.clone()),
                )
            }
            _ => ("-".to_string(), "not submitted".to_string(), None),
        })
        .collect();
    let state_width = cells
        .iter()
        .filter(|(_, _, base)| base.is_some())
        .map(|(_, state, _)| state.len())
        .max()
        .unwrap_or(0);

    let term = console::Term::stdout();
    for (row, (pr, state, base)) in rows.iter().zip(&cells).rev() {
        let title: String = if row.title.chars().count() > TITLE_WIDTH {
            let cut: String = row.title.chars().take(TITLE_WIDTH - 1).collect();
            format!("{cut}…")
        } else {
            row.title.clone()
        };
        let detail = match base {
            Some(base) => format!("{state:<state_width$}  → {base}"),
            None => state.clone(),
        };
        term.write_line(
            format!(
                "○  {}  {:<title_width$}  {:<pr_width$}  {}",
                console::style(short(&row.change_id)).magenta(),
                title,
                pr,
                detail,
            )
            .trim_end(),
        )?;
    }
    term.write_line(&format!("◆  {trunk}"))?;
    Ok(())
}
