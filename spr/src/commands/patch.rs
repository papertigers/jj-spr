/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr patch`: create local changes from existing Pull Requests, for
//! example to work on someone else's stack, and update them later when
//! someone pushes to the Pull Requests.
//!
//! When the Pull Request is part of a native GitHub stack, every open member
//! of the stack is fetched, bottom first. Each new change has its Pull
//! Request's diff and a `Pull Request:` trailer, so `jj spr diff` updates
//! the existing Pull Requests rather than opening new ones.
//!
//! A Pull Request that already has a local change is compared with it. The
//! change is updated from GitHub only when it has nothing that is not on
//! GitHub, and nothing is changed at all if a change and its Pull Request
//! both have edits the other lacks.

use std::collections::{HashMap, HashSet};

use git2::Oid;

use crate::{
    error::{Error, Result},
    github::{PullRequest, PullRequestState},
    message::{MessageSection, MessageSectionsMap, build_commit_message, parse_message},
    output::output,
    stacks::format_pr_list,
};

#[derive(Debug, clap::Parser)]
pub struct PatchOptions {
    /// Pull Request number. If it is part of a native GitHub stack, the
    /// whole stack is fetched.
    pull_request: u64,

    /// Only fetch this Pull Request, not the rest of its stack
    #[clap(long)]
    no_stack: bool,

    /// Create the changes without moving the working copy onto them
    #[clap(long)]
    no_checkout: bool,
}

/// What to do for one Pull Request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// No local change yet: create one.
    Create,
    /// The local change has nothing GitHub lacks, but GitHub has more.
    Update(String),
    /// The local change and GitHub have the same edits.
    UpToDate(String),
    /// The local change has edits that are not on GitHub yet, and GitHub has
    /// nothing it lacks.
    Ahead(String),
    /// Both have edits the other lacks.
    Diverged(String),
}

pub async fn patch(
    opts: PatchOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    output("🔄", &format!("Fetching from {}", config.remote_name))?;
    jj.run(["git", "fetch", "--remote", &config.remote_name])?;

    let numbers = if opts.no_stack {
        vec![opts.pull_request]
    } else {
        stack_members(gh, opts.pull_request).await?
    };
    if numbers.len() > 1 {
        output(
            "🧱",
            &format!(
                "#{} is part of a stack: {} (bottom first)",
                opts.pull_request,
                format_pr_list(&numbers)
            ),
        )?;
    }

    let tasks: Vec<_> = numbers
        .iter()
        .map(|&n| tokio::spawn(gh.clone().get_pull_request(n)))
        .collect();
    let mut pull_requests = Vec::new();
    for task in tasks {
        pull_requests.push(task.await??);
    }

    // Decide everything before changing anything.
    let linked = linked_changes(jj, config)?;
    let mut plans = Vec::new();
    for pr in &pull_requests {
        if pr.state != PullRequestState::Open {
            return Err(Error::new(format!(
                "Pull Request #{} is not open",
                pr.number
            )));
        }
        if pr.head_oid.is_zero() || jj.tree_if_present(pr.head_oid).is_none() {
            return Err(Error::new(format!(
                "Could not fetch branch {} of Pull Request #{} from {} (Pull Requests \
                 from forks are not supported)",
                pr.head.branch_name(),
                pr.number,
                config.remote_name
            )));
        }
        plans.push(match linked.get(&pr.number) {
            None => Plan::Create,
            Some(change) => compare(jj, config, pr, change)?,
        });
    }
    check_local_order(jj, config, &pull_requests, &plans)?;

    let diverged: Vec<String> = zip_plans(&pull_requests, &plans)
        .filter_map(|(pr, plan)| match plan {
            Plan::Diverged(change) => Some(format!("#{} ({})", pr.number, short(change))),
            _ => None,
        })
        .collect();
    if !diverged.is_empty() {
        return Err(Error::new(format!(
            "Not updating anything: {} changed both locally and on GitHub. Move your \
             local edits into a separate change (for example with `jj split`) and run \
             `jj spr patch` again, or, to replace what is on GitHub with your version, \
             run `jj spr diff --discard-remote-changes`.",
            diverged.join(", ")
        )));
    }

    let mut previous: Option<String> = None;
    let mut created_any = false;
    for (pr, plan) in zip_plans(&pull_requests, &plans) {
        let change = match plan {
            Plan::Create => {
                let change = create_change(jj, config, pr, previous.as_deref())?;
                output(
                    "📥",
                    &format!("#{}: created {} {}", pr.number, short(&change), pr.title),
                )?;
                created_any = true;
                change
            }
            Plan::Update(change) => {
                update_change(jj, config, pr, change)?;
                output(
                    "🔄",
                    &format!("#{}: updated {} from GitHub", pr.number, short(change)),
                )?;
                change.clone()
            }
            Plan::UpToDate(change) => {
                output(
                    "✅",
                    &format!("#{}: {} is up to date", pr.number, short(change)),
                )?;
                change.clone()
            }
            Plan::Ahead(change) => {
                output(
                    "📝",
                    &format!(
                        "#{}: {} has local edits that are not on GitHub yet; keeping \
                         them (`jj spr diff` pushes them)",
                        pr.number,
                        short(change)
                    ),
                )?;
                change.clone()
            }
            Plan::Diverged(_) => unreachable!("checked above"),
        };
        previous = Some(change);
    }
    let top = previous.expect("at least one Pull Request");

    let conflicted = jj.run([
        "log",
        "--no-graph",
        "-r",
        &format!("conflicts() & ::{top} & mutable()"),
        "-T",
        "change_id.short() ++ \" \" ++ description.first_line() ++ \"\\n\"",
    ])?;
    if !conflicted.trim().is_empty() {
        output(
            "⚠️",
            &format!(
                "These changes have conflicts, because a Pull Request is out of date with \
                 the one below it:\n{}",
                conflicted.trim_end()
            ),
        )?;
    }

    // Only move the working copy onto changes that are new.
    if created_any && !opts.no_checkout {
        jj.run(["new", &top])?;
        output(
            "👉",
            &format!("Working copy is now on top of {}", short(&top)),
        )?;
    }
    Ok(())
}

fn zip_plans<'a>(
    pull_requests: &'a [PullRequest],
    plans: &'a [Plan],
) -> impl Iterator<Item = (&'a PullRequest, &'a Plan)> {
    pull_requests.iter().zip(plans)
}

/// The open members of the native stack that `number` belongs to, bottom
/// first, or just `number` when it is not stacked.
async fn stack_members(gh: &crate::github::GitHub, number: u64) -> Result<Vec<u64>> {
    let stacks = gh.list_stacks().await?.unwrap_or_default();
    let members = stacks
        .iter()
        .map(|stack| stack.active_pull_requests())
        .find(|active| active.contains(&number));
    Ok(members.unwrap_or_else(|| vec![number]))
}

/// Where the Pull Request branched off its base.
fn fork_point(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
) -> Result<Oid> {
    let base = if pr.base_oid.is_zero() {
        jj.resolve_reference(config.master_ref.local())?
    } else {
        pr.base_oid
    };
    Ok(jj.git_repo.merge_base(pr.head_oid, base)?)
}

/// Compare a Pull Request with its existing local change.
///
/// GitHub has something new when `jj spr diff` would refuse to push over it
/// (see `has_remote_changes`). The local change has nothing unpushed when
/// its tree is one the Pull Request branch has had, or when its own diff is
/// already on GitHub.
fn compare(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
    change: &str,
) -> Result<Plan> {
    let local = jj.get_prepared_commit_for_revision(config, change)?;
    let local_tree = jj.get_tree_oid_for_commit(local.oid)?;
    let head_tree = jj.get_tree_oid_for_commit(pr.head_oid)?;

    let github_has_more = super::diff::has_remote_changes(jj, config, &local, pr)?;
    let local_is_pushed = branch_trees(jj, config, pr)?.contains(&local_tree)
        || jj.tree_contains_diff(
            head_tree,
            jj.get_tree_oid_for_commit(local.parent_oid)?,
            local_tree,
        )?;
    let change = change.to_string();
    Ok(match (github_has_more, local_is_pushed) {
        (false, true) => Plan::UpToDate(change),
        (false, false) => Plan::Ahead(change),
        (true, true) => Plan::Update(change),
        (true, false) => Plan::Diverged(change),
    })
}

/// The trees of the commits on the Pull Request branch since it branched
/// off its base.
fn branch_trees(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
) -> Result<HashSet<Oid>> {
    let mut walk = jj.git_repo.revwalk()?;
    walk.push(pr.head_oid)?;
    walk.hide(fork_point(jj, config, pr)?)?;
    let mut trees = HashSet::new();
    for oid in walk {
        trees.insert(jj.git_repo.find_commit(oid?)?.tree_id());
    }
    Ok(trees)
}

/// Existing local changes must be stacked like their Pull Requests, and any
/// Pull Request without a local change must be above all of those that have
/// one.
fn check_local_order(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pull_requests: &[PullRequest],
    plans: &[Plan],
) -> Result<()> {
    let mut below: Option<(u64, Oid)> = None;
    let mut missing: Option<u64> = None;
    for (pr, plan) in zip_plans(pull_requests, plans) {
        let change = match plan {
            Plan::Create => {
                missing.get_or_insert(pr.number);
                continue;
            }
            Plan::Update(c) | Plan::UpToDate(c) | Plan::Ahead(c) | Plan::Diverged(c) => c,
        };
        if let Some(missing) = missing {
            return Err(Error::new(format!(
                "Pull Request #{missing} has no local change, but #{} above it does \
                 ({}). Nothing was changed.",
                pr.number,
                short(change)
            )));
        }
        let local = jj.get_prepared_commit_for_revision(config, change)?;
        if let Some((number, oid)) = below
            && local.parent_oid != oid
        {
            return Err(Error::new(format!(
                "The local change for #{} ({}) is not on top of the one for #{number}, \
                 as it is on GitHub. Rebase it there first (`jj rebase`). Nothing was \
                 changed.",
                pr.number,
                short(change)
            )));
        }
        below = Some((pr.number, local.oid));
    }
    Ok(())
}

/// Create one local change for `pr` and return its change ID.
fn create_change(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
    parent: Option<&str>,
) -> Result<String> {
    let message = build_commit_message(&commit_message(pr));
    new_change_with_pr_diff(jj, config, pr, parent, &message)
}

/// Replace an existing change's content with its Pull Request's diff,
/// applied on the change's current parent. The change keeps its change ID,
/// description, and any descendants.
fn update_change(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
    change: &str,
) -> Result<()> {
    let scratch = new_change_with_pr_diff(
        jj,
        config,
        pr,
        Some(&format!("{change}-")),
        &format!("jj-spr patch: updating #{}", pr.number),
    )?;
    jj.run(["restore", "--quiet", "--from", &scratch, "--into", change])?;
    jj.run(["abandon", "--quiet", &scratch])?;
    Ok(())
}

/// Create a change holding the Pull Request's own diff, on top of
/// `parent` (a revision), or where the Pull Request branched off its base
/// if there is none. Returns its change ID.
///
/// The change starts at the fork point and gets the Pull Request's tree, so
/// its diff is exactly the Pull Request's; it is then rebased onto `parent`.
/// That is a no-op for the tree when the Pull Request contains its base (as
/// `jj spr diff` makes sure), and otherwise applies the Pull Request's own
/// diff, just as GitHub shows it.
fn new_change_with_pr_diff(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pr: &PullRequest,
    parent: Option<&str>,
    message: &str,
) -> Result<String> {
    let fork = fork_point(jj, config, pr)?.to_string();

    // jj's own messages are left out (`--quiet`): they would call each
    // change empty before its tree is restored.
    let change = new_change(jj, &fork, message)?;
    jj.run([
        "restore",
        "--quiet",
        "--from",
        &pr.head_oid.to_string(),
        "--into",
        &change,
    ])?;
    if let Some(parent) = parent {
        jj.run(["rebase", "--quiet", "-r", &change, "-d", parent])?;
    }
    Ok(change)
}

/// `jj new --no-edit`, returning the new change's ID.
fn new_change(jj: &crate::jj::Jujutsu, parent: &str, message: &str) -> Result<String> {
    let children = || -> Result<HashSet<String>> {
        Ok(jj
            .run([
                "log",
                "--no-graph",
                "-r",
                &format!("children({parent})"),
                "-T",
                "change_id ++ \"\\n\"",
            ])?
            .lines()
            .map(str::to_string)
            .collect())
    };
    let before = children()?;
    jj.run(["new", "--quiet", "--no-edit", parent, "-m", message])?;
    let mut created: Vec<String> = children()?.difference(&before).cloned().collect();
    match created.len() {
        1 => Ok(created.remove(0)),
        _ => Err(Error::new(format!(
            "could not find the change jj created on {parent}"
        ))),
    }
}

/// The Pull Request's title and description, with its `Pull Request:`
/// trailer. Reviewers are left out: they are the author's to manage.
fn commit_message(pr: &PullRequest) -> MessageSectionsMap {
    let mut message = MessageSectionsMap::new();
    for section in [
        MessageSection::Title,
        MessageSection::Summary,
        MessageSection::PullRequest,
    ] {
        if let Some(text) = pr.sections.get(&section).filter(|t| !t.is_empty()) {
            message.insert(section, text.clone());
        }
    }
    message
}

/// Map Pull Request numbers to the mutable changes that name them in a
/// `Pull Request:` trailer.
fn linked_changes(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
) -> Result<HashMap<u64, String>> {
    let listing = jj.run([
        "log",
        "--no-graph",
        "-r",
        "mutable() & description(substring-i:\"pull request:\")",
        "-T",
        "change_id ++ \"\\0\" ++ description ++ \"\\x1e\"",
    ])?;
    Ok(listing
        .split('\x1e')
        .filter_map(|entry| {
            let (change, description) = entry.split_once('\0')?;
            let number = parse_message(description, MessageSection::Title)
                .get(&MessageSection::PullRequest)
                .and_then(|field| config.parse_pull_request_field(field))?;
            Some((number, change.trim().to_string()))
        })
        .collect())
}

fn short(change_id: &str) -> &str {
    &change_id[..change_id.len().min(8)]
}
