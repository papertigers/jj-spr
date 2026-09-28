/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr sync`: bring a local stack up to date after some of its Pull
//! Requests have merged.
//!
//! It fetches, abandons the local changes whose Pull Requests merged, and
//! rebases what is left of the stack onto the main branch. Nothing is
//! changed unless every merged change can be abandoned safely, and it never
//! rebases merely because the main branch moved; `jj rebase` owns that.

use git2::Oid;

use crate::{
    error::{Error, Result},
    github::PullRequestState,
    jj::{PreparedCommit, short},
    output::output,
};

#[derive(Debug, clap::Parser)]
pub struct SyncOptions {
    /// Top of the stack to sync. Every change between the main branch and
    /// this revision is checked. Defaults to '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Show what would happen without changing anything (the remote is still
    /// fetched)
    #[clap(long)]
    dry_run: bool,
}

/// What `sync` found for one local change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    /// No Pull Request yet.
    Unsubmitted,
    Open(u64),
    /// Merged, and the local change is exactly what was merged.
    Merged(u64),
    /// Merged, but the local change cannot be abandoned safely.
    MergedWithDifferences(u64, String),
    /// Closed without merging.
    ClosedUnmerged(u64),
}

struct Change {
    commit: PreparedCommit,
    change_id: String,
    status: Status,
}

impl Change {
    fn label(&self) -> String {
        let title = self
            .commit
            .message
            .get(&crate::message::MessageSection::Title)
            .map(String::as_str)
            .unwrap_or("(no title)");
        format!("{} {}", short(&self.change_id), title)
    }
}

pub async fn sync(
    opts: SyncOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let trunk = config.trunk_revset();
    let target = opts.revision.as_deref().unwrap_or("@-");

    output("🔄", &format!("Fetching from {}", config.remote_name))?;
    jj.run(["git", "fetch", "--remote", &config.remote_name])?;

    let commits = jj.get_prepared_commits_from_to(config, &trunk, target, false)?;
    if commits.is_empty() {
        output(
            "👋",
            &format!("No changes between {trunk} and {target}. Nothing to sync."),
        )?;
        return Ok(());
    }
    jj.require_linear(&commits, "syncing")?;

    let change_ids = jj.change_ids_for(&commits)?;
    let pull_requests = gh
        .get_pull_requests(commits.iter().map(|c| c.pull_request_number))
        .await?;
    let mut changes = Vec::new();
    for (commit, pull_request) in commits.into_iter().zip(pull_requests) {
        let status = match pull_request {
            None => Status::Unsubmitted,
            Some(pull_request) => status_of(jj, config, &commit, &pull_request).await?,
        };
        let change_id = change_ids
            .get(&commit.oid)
            .cloned()
            .ok_or_else(|| Error::new(format!("could not find the change ID of {}", commit.oid)))?;
        changes.push(Change {
            commit,
            change_id,
            status,
        });
    }

    for change in &changes {
        match &change.status {
            Status::Merged(n) => output("🛬", &format!("{}: #{n} merged", change.label()))?,
            Status::Open(n) => output("📖", &format!("{}: #{n} is open", change.label()))?,
            Status::Unsubmitted => output("📝", &format!("{}: not submitted", change.label()))?,
            Status::ClosedUnmerged(n) => output(
                "📕",
                &format!(
                    "{}: #{n} was closed without merging; keeping it",
                    change.label()
                ),
            )?,
            Status::MergedWithDifferences(n, why) => {
                output("⚠️", &format!("{}: #{n} merged, but {why}", change.label()))?
            }
        }
    }

    // Stop before changing anything if a merged change cannot be abandoned.
    let blocked: Vec<&Change> = changes
        .iter()
        .filter(|c| matches!(c.status, Status::MergedWithDifferences(..)))
        .collect();
    if !blocked.is_empty() {
        return Err(Error::new(format!(
            "Not syncing: {} merged, but the local version differs from what merged. \
             Move any edits you want to keep into another change (for example with \
             `jj split` or `jj squash --into`), or abandon the change with `jj abandon` \
             if you don't need them, then run `jj spr sync` again.",
            blocked
                .iter()
                .map(|c| short(&c.change_id).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let merged: Vec<&Change> = changes
        .iter()
        .filter(|c| matches!(c.status, Status::Merged(_)))
        .collect();
    if merged.is_empty() {
        output("✅", "Nothing in this stack has merged. Nothing to do.")?;
        return Ok(());
    }
    let remaining: Vec<&Change> = changes
        .iter()
        .filter(|c| !matches!(c.status, Status::Merged(_)))
        .collect();

    if opts.dry_run {
        for change in &merged {
            output("  ", &format!("Would abandon {}", change.label()))?;
        }
        if let Some(root) = remaining.first() {
            output(
                "  ",
                &format!(
                    "Would rebase {} and its descendants onto {trunk}",
                    short(&root.change_id)
                ),
            )?;
        }
        return Ok(());
    }

    let mut abandon = vec!["abandon".to_string()];
    abandon.extend(merged.iter().map(|c| c.change_id.clone()));
    jj.run(&abandon)?;
    output(
        "🗑️ ",
        &format!(
            "Abandoned {}",
            merged
                .iter()
                .map(|c| short(&c.change_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )?;

    let (Some(root), Some(top)) = (remaining.first(), remaining.last()) else {
        output("🎉", "Every change in this stack has merged.")?;
        return Ok(());
    };

    jj.run(["rebase", "-s", &root.change_id, "-d", &trunk])?;
    output(
        "⤴️ ",
        &format!("Rebased {} change(s) onto {trunk}", remaining.len()),
    )?;

    let conflicted = jj.conflicted_changes(&format!("{trunk}..{}", top.change_id))?;
    if !conflicted.is_empty() {
        return Err(Error::new(format!(
            "The rebase left conflicts in:\n{conflicted}\nResolve them (for example with \
             `jj resolve`) before running `jj spr diff --all`, or undo the sync with `jj undo`.",
        )));
    }

    if remaining
        .iter()
        .any(|c| matches!(c.status, Status::Open(_)))
    {
        output(
            "👉",
            "Run `jj spr diff --all` to update the remaining Pull Requests.",
        )?;
    }
    Ok(())
}

/// Classify a submitted change by its Pull Request.
async fn status_of(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    commit: &PreparedCommit,
    pull_request: &crate::github::PullRequest,
) -> Result<Status> {
    let number = pull_request.number;
    match pull_request.state {
        PullRequestState::Open => return Ok(Status::Open(number)),
        PullRequestState::Closed => return Ok(Status::ClosedUnmerged(number)),
        PullRequestState::Merged => {}
    }

    // The Pull Request merged. Abandoning the local change is only safe if
    // the merged Pull Request already contains everything the change does.
    let Some(head) = pull_request.github_head_oid else {
        return Ok(Status::MergedWithDifferences(
            number,
            "GitHub did not report its final version".into(),
        ));
    };
    if jj.tree_if_present(head).is_none() {
        // Not in the local repository (for example, it was pushed from
        // another machine). GitHub still serves it by ID.
        let _ = crate::utils::run_command(
            jj.git_command()
                .arg("fetch")
                .arg("--no-write-fetch-head")
                .arg("--")
                .arg(&config.remote_name)
                .arg(head.to_string()),
        )
        .await;
    }
    let Some(head_tree) = jj.tree_if_present(head) else {
        return Ok(Status::MergedWithDifferences(
            number,
            "its final version could not be fetched to compare with".into(),
        ));
    };
    if !already_contains(jj, head_tree, commit)? {
        return Ok(Status::MergedWithDifferences(
            number,
            "the local change has edits that are not in the merged Pull Request".into(),
        ));
    }
    Ok(Status::Merged(number))
}

/// Whether the tree `published` already contains everything `change` does.
///
/// Comparing whole trees is not enough: a cherry-picked Pull Request, or one
/// whose branch was updated from main on GitHub, has a different tree from
/// the local change even though nothing is unpublished. Instead, check that
/// the local change's own diff (from its parent to itself) is already in the
/// published tree.
fn already_contains(
    jj: &crate::jj::Jujutsu,
    published: Oid,
    change: &PreparedCommit,
) -> Result<bool> {
    jj.tree_contains_diff(
        published,
        jj.get_tree_oid_for_commit(change.parent_oid)?,
        jj.get_tree_oid_for_commit(change.oid)?,
    )
}
