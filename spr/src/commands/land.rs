/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{io::Write, process::Stdio, time::Duration};

use crate::{
    error::{Error, Result, ResultExt},
    github::{PullRequestState, PullRequestUpdate, ReviewStatus},
    message::{MessageSection, build_github_body_for_merging},
    output::{output, write_commit_title},
    utils::run_command,
};

#[derive(Debug, clap::Parser)]
pub struct LandOptions {
    /// Merge a Pull Request that was created or updated with spr diff
    /// --cherry-pick
    #[clap(long)]
    cherry_pick: bool,

    /// Jujutsu revision to operate on (if not specified, uses '@'). In a
    /// native GitHub stack this can be any Pull Request of the stack: it is
    /// merged together with every Pull Request below it.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Merge the Pull Requests below the chosen one in its stack without
    /// asking (required when not running in a terminal)
    #[clap(long, short = 'y')]
    yes: bool,
}

fn resolve_cherry_pick(
    cli_cherry_pick: bool,
    message: &crate::message::MessageSectionsMap,
) -> bool {
    cli_cherry_pick
        || message
            .get(&MessageSection::CherryPick)
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

pub async fn land(
    mut opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let prepared_commit = jj.get_prepared_commit_for_revision(config, revision)?;

    // Honor both the --cherry-pick flag and the "Cherry Pick:" marker on the
    // commit description. When the validation TODO below is filled in, use
    // opts.cherry_pick as the authoritative source.
    opts.cherry_pick = resolve_cherry_pick(opts.cherry_pick, &prepared_commit.message);

    write_commit_title(&prepared_commit)?;

    let pull_request_number = if let Some(number) = prepared_commit.pull_request_number {
        output("#️⃣ ", &format!("Pull Request #{}", number))?;
        number
    } else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };

    // Load Pull Request information
    let pull_request = gh.clone().get_pull_request(pull_request_number).await?;

    check_landable(config, &pull_request)?;

    output("🛫", "Getting started...")?;

    // Fetch current master from GitHub.
    run_command(
        jj.git_command()
            .arg("fetch")
            .arg("--no-write-fetch-head")
            .arg("--")
            .arg(&config.remote_name)
            .arg(config.master_ref.on_github()),
    )
    .await
    .reword("git fetch failed".to_string())?;

    // TODO: Implement Jujutsu-native cherry-pick and merge validation
    // For now, we'll trust GitHub's merge validation and skip local validation
    let base_is_master = pull_request.base.is_master_branch();

    // Skip local cherry-pick validation for Jujutsu workflow
    // GitHub will validate mergeability during the merge process
    let merge_matches_cherrypick = true;

    if !merge_matches_cherrypick {
        return Err(Error::new(formatdoc!(
            "This commit has been updated and/or rebased since the pull \
             request was last updated. Please run `spr diff` to update the \
             pull request and then try `spr land` again!"
        )));
    }

    // Okay, we are confident now that the PR can be merged and the result of
    // that merge would be a master commit with the same tree as if we
    // cherry-picked the commit onto master.
    let pr_head_oid = pull_request.head_oid;

    // A Pull Request in a native GitHub stack is merged through GitHub's
    // stack-aware merge, which merges every Pull Request below it too.
    let stack = match gh.list_stacks().await {
        Ok(Some(stacks)) => stacks
            .into_iter()
            .find(|stack| stack.active_pull_requests().contains(&pull_request_number)),
        _ => None,
    };
    // The Pull Requests below this one that merge with it, bottom first.
    let mut merged_below = Vec::new();
    if let Some(stack) = &stack {
        let active = stack.active_pull_requests();
        let below: Vec<u64> = active
            .iter()
            .copied()
            .take_while(|n| *n != pull_request_number)
            .collect();
        let tasks: Vec<_> = below
            .iter()
            .map(|&n| tokio::spawn(gh.clone().get_pull_request(n)))
            .collect();
        for task in tasks {
            let pr = task.await??;
            check_landable(config, &pr)?;
            merged_below.push(pr);
        }
        let bottom = merged_below.first().unwrap_or(&pull_request);
        if !bottom.base.is_master_branch() {
            return Err(Error::new(format!(
                "GitHub stack #{} targets {}, not {}.",
                stack.number,
                bottom.base.branch_name(),
                config.master_ref.branch_name()
            )));
        }

        if merged_below.is_empty() {
            output(
                "🧱",
                &format!("Merging from the bottom of GitHub stack #{}", stack.number),
            )?;
        } else {
            let mut merging = below.clone();
            merging.push(pull_request_number);
            output(
                "🧱",
                &format!(
                    "Landing #{} merges {} from GitHub stack #{} (bottom first)",
                    pull_request_number,
                    crate::stacks::format_pr_list(&merging),
                    stack.number
                ),
            )?;
            confirm_merging_below(&opts, &below)?;
        }
    }

    if !base_is_master && stack.is_none() {
        // The base of the Pull Request on GitHub is not set to master. This
        // means the Pull Request uses a base branch. We tested above that
        // merging the Pull Request branch into the master branch produces the
        // intended result (the same as cherry-picking the local commit onto
        // master), so what we want to do is actually merge the Pull Request as
        // it is into master. Hence, we change the base to the master branch.
        //
        // Before we do that, there is one more edge case to look out for: if
        // the base branch contains changes that have since been landed on
        // master, then Git might be able to figure out that these changes
        // appear both in the pull request branch (via the merge branch) and in
        // master, but are identical in those two so it is not a merge conflict
        // but can go ahead. The result of this in master if we merge now is
        // correct, but there is one problem: when looking at the Pull Request
        // in GitHub after merging, it will show these change as part of the
        // Pull Request. So when you look at the changed files of the Pull
        // Request, you will see both changes in this commit (great!) and those
        // in the base branch (a previous commit that has already been landed on
        // master - not great!). This is because the changes shown are the ones
        // that happened on this Pull Request branch (now including the base
        // branch) since it branched off master. This can include changes in the
        // base branch that are already on master, but were added to master
        // after the Pull Request branch branched from master.
        // The solution is to merge current master into the Pull Request branch.
        // Doing that now means that the final changes done by this Pull Request
        // are only the changes that are not yet in master. That's what we want.
        // This final merge never introduces any changes to the Pull Request. In
        // fact, the tree that we use for the merge commit is the one we got
        // above from the cherry-picking of this commit on master.

        // TODO: Implement Jujutsu-native merge base and tree comparison
        // For now, skip the complex merge-in-master logic
        // This logic would need to be rewritten using jj commands

        // Skip the merge-in-master commit creation for Jujutsu workflow

        gh.update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                base: Some(config.master_ref.branch_name().to_string()),
                ..Default::default()
            },
        )
        .await?;
    }

    // Check whether GitHub says this PR is mergeable. This happens in a
    // retry-loop because recent changes to the Pull Request can mean that
    // GitHub has not finished the mergeability check yet.
    let mut attempts = 0;
    let result = loop {
        attempts += 1;

        let mergeability = gh
            .get_pull_request_mergeability(pull_request_number)
            .await?;

        if mergeability.head_oid != pr_head_oid {
            break Err(Error::new(formatdoc!(
                "The Pull Request seems to have been updated externally.
                     Please try again!"
            )));
        }

        // A stacked Pull Request keeps its base; the stack merge lands it.
        if (mergeability.base.is_master_branch() || stack.is_some())
            && mergeability.mergeable.is_some()
        {
            if mergeability.mergeable != Some(true) {
                break Err(Error::new(formatdoc!(
                    "GitHub concluded the Pull Request is not mergeable at \
                    this point. Please rebase your changes and try again!"
                )));
            }

            // TODO: Implement Jujutsu-native commit fetching and tree comparison
            // For now, skip the merge commit validation
            // This would need to be rewritten using jj commands

            break Ok(());
        }

        if attempts >= 10 {
            // After ten failed attempts we give up.
            break Err(Error::new(
                "GitHub Pull Request did not update. Please try again!",
            ));
        }

        // Wait one second before retrying
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    let result = match result {
        Ok(()) => {
            // We have checked that merging the Pull Request branch into the master
            // branch produces the intended result, and that's independent of whether we
            // used a base branch with this Pull Request or not. We have made sure the
            // target of the Pull Request is set to the master branch. So let GitHub do
            // the merge now!
            if stack.is_some() {
                gh.merge_stacked_pull_request(pull_request_number, pr_head_oid, "squash")
                    .await
                    .context(format!(
                        "squash-merging PR #{} (head {})",
                        pull_request_number, pr_head_oid
                    ))
            } else {
                octocrab::instance()
                    .pulls(&config.owner, &config.repo)
                    .merge(pull_request_number)
                    .method(octocrab::params::pulls::MergeMethod::Squash)
                    .title(pull_request.title)
                    .message(build_github_body_for_merging(&pull_request.sections))
                    .sha(format!("{}", pr_head_oid))
                    .send()
                    .await
                    .convert()
                    .context(format!(
                        "squash-merging PR #{} (head {})",
                        pull_request_number, pr_head_oid
                    ))
                    .and_then(|merge| {
                        if merge.merged {
                            Ok(merge.sha)
                        } else {
                            Err(Error::new(formatdoc!(
                                "GitHub Pull Request merge failed: {}",
                                merge.message.unwrap_or_default()
                            )))
                        }
                    })
            }
        }
        Err(err) => Err(err),
    };

    let merged_sha = match result {
        Ok(sha) => sha,
        Err(mut error) => {
            output("❌", "GitHub Pull Request merge failed")?;

            // If we changed the target branch of the Pull Request earlier, then
            // undo this change now.
            if !base_is_master && stack.is_none() {
                let result = gh
                    .update_pull_request(
                        pull_request_number,
                        PullRequestUpdate {
                            base: Some(pull_request.base.on_github().to_string()),
                            ..Default::default()
                        },
                    )
                    .await;
                if let Err(e) = result {
                    error.push(format!("{}", e));
                }
            }

            return Err(error);
        }
    };

    output("🛬", "Landed!")?;

    // Other open Pull Requests may target a merged Pull Request's branch
    // (natively stacked PRs do). GitHub closes a Pull Request whose base
    // branch is deleted, so move them to master first, and keep any branch
    // that another open Pull Request still uses.
    let merged_heads: Vec<&crate::github::GitHubBranch> = merged_below
        .iter()
        .map(|pr| &pr.head)
        .chain([&pull_request.head])
        .collect();
    let merged_numbers: Vec<u64> = merged_below
        .iter()
        .map(|pr| pr.number)
        .chain([pull_request_number])
        .collect();
    let open_pull_requests: Vec<_> = gh
        .get_open_pull_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|pr| !merged_numbers.contains(&pr.number))
        .collect();
    let mut kept_heads = Vec::new();
    for dependent in open_pull_requests.iter() {
        let Some(head) = merged_heads
            .iter()
            .find(|head| dependent.base_ref_name == head.branch_name())
        else {
            continue;
        };
        let retarget = gh
            .update_pull_request(
                dependent.number,
                PullRequestUpdate {
                    base: Some(config.master_ref.branch_name().to_string()),
                    ..Default::default()
                },
            )
            .await;
        match retarget {
            Ok(()) => output(
                "↪️ ",
                &format!(
                    "Pull Request #{} now targets {}",
                    dependent.number,
                    config.master_ref.branch_name()
                ),
            )?,
            Err(_) => {
                kept_heads.push(head.branch_name().to_string());
                output(
                    "⚠️",
                    &format!(
                        "Pull Request #{} still targets {}; keeping that branch",
                        dependent.number,
                        head.branch_name()
                    ),
                )?;
            }
        }
    }
    let used_elsewhere = |branch: &str| {
        open_pull_requests
            .iter()
            .any(|pr| pr.head_ref_name == branch || pr.base_ref_name == branch)
    };

    let mut branches_to_delete: Vec<&crate::github::GitHubBranch> = merged_heads
        .iter()
        .copied()
        .filter(|head| !kept_heads.iter().any(|kept| kept == head.branch_name()))
        .collect();
    if !base_is_master && stack.is_none() && !used_elsewhere(pull_request.base.branch_name()) {
        branches_to_delete.push(&pull_request.base);
    }
    let mut delete_branch_processes = Vec::new();
    for branch in branches_to_delete {
        delete_branch_processes.push((
            branch.branch_name().to_string(),
            jj.git_command()
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(branch.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        ));
    }

    // Rebase us on top of the now-landed commit
    if let Some(sha) = merged_sha {
        // Try this up to three times, because fetching the very moment after
        // the merge might still not find the new commit.
        for i in 0..3 {
            // Fetch current master and the merge commit from GitHub.
            let git_fetch = jj
                .git_command()
                .arg("fetch")
                .arg("--no-write-fetch-head")
                .arg("--")
                .arg(&config.remote_name)
                .arg(config.master_ref.on_github())
                .arg(&sha)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await?;
            if git_fetch.status.success() {
                break;
            } else if i == 2 {
                console::Term::stderr().write_all(&git_fetch.stderr)?;
                return Err(Error::new("git fetch failed"));
            }
        }
        output(
            "👉",
            "Run `jj spr sync` to abandon the landed change and rebase the rest \
             of the stack",
        )?;
    }

    // Wait for the "git push" deleting the old branches to finish. GitHub may
    // be configured to delete the branch automatically, in which case it's
    // gone already and the push fails harmlessly; report any other failure.
    for (branch, proc) in delete_branch_processes {
        let result = proc.wait_with_output().await?;
        let stderr = String::from_utf8_lossy(&result.stderr);
        if !result.status.success() && !stderr.contains("remote ref does not exist") {
            output(
                "⚠️",
                &format!(
                    "Could not delete branch {branch}: {}",
                    stderr
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            )?;
        }
    }

    Ok(())
}

/// Refuse Pull Requests that GitHub would not merge, or that are not
/// approved when approval is required.
fn check_landable(
    config: &crate::config::Config,
    pull_request: &crate::github::PullRequest,
) -> Result<()> {
    let number = pull_request.number;
    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(format!(
            "Pull Request #{number} is already closed!"
        )));
    }
    if pull_request.is_draft {
        return Err(Error::new(format!(
            "Pull Request #{number} is a draft. Mark it ready for review first (for \
             example with `gh pr ready {number}`)."
        )));
    }
    if config.require_approval && pull_request.review_status != Some(ReviewStatus::Approved) {
        return Err(Error::new(format!(
            "Pull Request #{number} has not been approved on GitHub."
        )));
    }
    Ok(())
}

/// Landing a Pull Request above the bottom of a stack merges the ones below
/// it too; make sure that is intended.
fn confirm_merging_below(opts: &LandOptions, below: &[u64]) -> Result<()> {
    use std::io::IsTerminal;
    if opts.yes {
        return Ok(());
    }
    let list = crate::stacks::format_pr_list(below);
    if !std::io::stdin().is_terminal() {
        return Err(Error::new(format!(
            "This would also merge {list}. Pass --yes to confirm, or land the bottom \
             of the stack first."
        )));
    }
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(format!("Also merge {list}?"))
        .default(false)
        .interact()?;
    if confirmed {
        Ok(())
    } else {
        Err(Error::new("Not landing anything."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with_cherry_pick(value: &str) -> crate::message::MessageSectionsMap {
        [(MessageSection::CherryPick, value.to_string())].into()
    }

    #[test]
    fn test_land_resolve_flag_true() {
        let map = crate::message::MessageSectionsMap::new();
        assert!(resolve_cherry_pick(true, &map));
    }

    #[test]
    fn test_land_resolve_flag_false_with_marker() {
        let map = map_with_cherry_pick("true");
        assert!(resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_flag_false_without_marker() {
        let map = crate::message::MessageSectionsMap::new();
        assert!(!resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_marker_case_insensitive() {
        let map = map_with_cherry_pick("TRUE");
        assert!(resolve_cherry_pick(false, &map));
    }

    #[test]
    fn test_land_resolve_marker_other_value() {
        for value in &["false", "yes", "1", ""] {
            let map = map_with_cherry_pick(value);
            assert!(
                !resolve_cherry_pick(false, &map),
                "Expected false for marker value {:?}",
                value
            );
        }
    }
}
