/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::iter::zip;

use crate::{
    error::{Error, Result, ResultExt, add_error},
    github::{
        GitHub, PullRequest, PullRequestRequestReviewers, PullRequestState, PullRequestUpdate,
    },
    message::{MessageSection, validate_commit_message},
    output::{output, write_commit_title},
    utils::{parse_name_list, remove_all_parens, run_command},
};
use git2::Oid;
use indoc::{formatdoc, indoc};

#[derive(Debug, clap::Parser)]
pub struct DiffOptions {
    /// Create/update pull requests for commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Update the pull request title and description on GitHub from the local
    /// commit message
    #[clap(long)]
    update_message: bool,

    /// Submit any new Pull Request as a draft
    #[clap(long)]
    draft: bool,

    /// Message to be used for commits updating existing pull requests (e.g.
    /// 'rebase' or 'review comments')
    #[clap(long, short = 'm')]
    message: Option<String>,

    /// Submit this commit as if it was cherry-picked on master. Do not base it
    /// on any intermediate changes between the master branch and this commit.
    #[clap(long)]
    cherry_pick: bool,

    /// Remove the "Cherry Pick:" marker from the commit description and ignore
    /// it for this invocation. Future `jj spr diff` invocations will behave
    /// as though --cherry-pick was not specified.
    #[clap(long, conflicts_with = "cherry_pick")]
    no_cherry_pick: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Preview what would happen without pushing or creating PRs
    #[clap(long)]
    pub dry_run: bool,

    /// Publish dependent changes as a native GitHub stack: each Pull Request
    /// targets the Pull Request branch of its parent change, and the chain is
    /// registered with GitHub's stacked pull requests. Defaults to the
    /// `spr.nativeStacks` setting.
    #[clap(long)]
    native_stack: bool,

    /// Use jj-spr's own base branches for dependent changes, even if
    /// `spr.nativeStacks` is set.
    #[clap(long, conflicts_with = "native_stack")]
    no_native_stack: bool,

    /// Push even if a Pull Request has commits that are not in its local
    /// change (someone else pushed to it). Their edits stay in the branch
    /// history but disappear from the Pull Request's diff.
    #[clap(long)]
    discard_remote_changes: bool,
}

/// The Pull Request branch of a change's parent, which a natively stacked
/// Pull Request targets.
#[derive(Debug, Clone)]
struct ParentPullRequest {
    number: Option<u64>,
    branch: crate::github::GitHubBranch,
    head_oid: Oid,
}

/// What `diff_impl` published (or would publish, in a dry run) for one change.
#[derive(Debug, Clone)]
struct Published {
    number: Option<u64>,
    head: crate::github::GitHubBranch,
    head_oid: Oid,
    /// The base branch, when this run set or confirmed it. `None` for a
    /// parent Pull Request that was only looked up.
    base: Option<String>,
}

/// Whether the change will be submitted as a cherry-pick, without changing
/// its message (see `resolve_cherry_pick`).
fn wants_cherry_pick(opts: &DiffOptions, message: &crate::message::MessageSectionsMap) -> bool {
    if opts.no_cherry_pick {
        return false;
    }
    opts.cherry_pick || crate::message::cherry_pick_marked(message)
}

/// `ancestor` is `descendant` or one of its ancestors.
fn is_ancestor(jj: &crate::jj::Jujutsu, ancestor: Oid, descendant: Oid) -> Result<bool> {
    Ok(ancestor == descendant || jj.git_repo.graph_descendant_of(descendant, ancestor)?)
}

/// Resolve the effective cherry-pick state from CLI flags and the "Cherry Pick:"
/// marker on the commit description.
///
/// When `--cherry-pick` is passed the marker is added (if absent); when
/// `--no-cherry-pick` is passed the marker is removed (if present). Returns
/// `(effective_cherry_pick, marker_was_changed)`.
fn resolve_cherry_pick(
    cherry_pick: bool,
    no_cherry_pick: bool,
    message: &mut crate::message::MessageSectionsMap,
) -> (bool, bool) {
    let stored = crate::message::cherry_pick_marked(message);

    if no_cherry_pick {
        if stored {
            message.remove(&MessageSection::CherryPick);
            return (false, true);
        }
        (false, false)
    } else if cherry_pick {
        if !stored {
            message.insert(MessageSection::CherryPick, "true".to_string());
            return (true, true);
        }
        (true, false)
    } else {
        (stored, false)
    }
}

pub async fn diff(
    opts: DiffOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let mut result = Ok(());

    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            opts.revision.as_deref(),
            opts.all,
            opts.base.as_deref(),
        )?;

    // Get commits to process
    let mut prepared_commits = if use_range_mode {
        // Get range of commits from base to target
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        // Just get the single specified revision
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    // Determine the master base OID - this is the commit on master that the stack is based on
    let master_base_oid = if let Some(first_commit) = prepared_commits.first() {
        if use_range_mode {
            // For range mode, the parent of the first commit is the master base
            first_commit.parent_oid
        } else {
            // For single commit mode, find the actual merge base with master
            jj.get_master_base_for_commit(config, first_commit.oid)?
        }
    } else {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return result;
    };

    let native = config.native_stacks_with(opts.native_stack, opts.no_native_stack);

    let pull_requests = gh
        .get_pull_requests(prepared_commits.iter().map(|pc| pc.pull_request_number))
        .await?;

    // Everything below compares against the Pull Requests' heads as fetched;
    // stop if one could not be fetched as GitHub reports it.
    for pr in pull_requests.iter().flatten() {
        if let Some(github_head) = pr.github_head_oid
            && pr.state == PullRequestState::Open
            && github_head != pr.head_oid
        {
            return Err(Error::new(format!(
                "Could not fetch the current head of Pull Request #{} (GitHub reports {}, \
                 the local copy of {} is {}). Run `jj git fetch` and try again.",
                pr.number,
                github_head,
                pr.head.branch_name(),
                pr.head_oid
            )));
        }
    }

    // Pushing replaces a Pull Request's content with the local change's, so
    // stop before changing anything if that would drop someone else's work.
    if !opts.discard_remote_changes {
        let mut changed_remotely = Vec::new();
        for (commit, pull_request) in zip(prepared_commits.iter(), pull_requests.iter()) {
            if let Some(pr) = pull_request
                && pr.state == PullRequestState::Open
                && !wants_cherry_pick(&opts, &commit.message)
                && jj.pr_has_remote_changes(config, commit, pr)?
            {
                changed_remotely.push(pr.number);
            }
        }
        if let Some(first) = changed_remotely.first() {
            let (has, s, them) = if changed_remotely.len() == 1 {
                ("has", "", "it")
            } else {
                ("have", "s", "them")
            };
            return Err(Error::new(format!(
                "{} {has} commits that are not in the local change{s} (someone else \
                 pushed to {them}). Run `jj spr patch {first}` to update your local \
                 changes from GitHub, or pass --discard-remote-changes to replace \
                 them with your version.",
                crate::stacks::format_pr_list(&changed_remotely),
            )));
        }
    }

    // With native stacks, GitHub refuses to change the base of a Pull Request
    // that is part of a stack. Work out up front which stacks this run is
    // about to invalidate, and dissolve them before changing anything; they
    // are rebuilt once every Pull Request is published.
    if native
        && use_range_mode
        && let Some(stacks) = gh.list_stacks().await?
    {
        let planned = plan_native_bases(
            &opts,
            config,
            &prepared_commits,
            &pull_requests,
            master_base_oid,
        );
        let to_dissolve = crate::stacks::plan_unstacks(&stacks, &planned).map_err(Error::new)?;
        for stack in to_dissolve {
            if opts.dry_run {
                output(
                    "🧱",
                    &format!("Would dissolve GitHub stack #{stack} to rebuild it"),
                )?;
            } else {
                output(
                    "🧱",
                    &format!("Dissolving GitHub stack #{stack} to rebuild it"),
                )?;
                gh.unstack(stack).await?;
            }
        }
    }

    let mut message_on_prompt = "".to_string();
    let mut parent: Option<ParentPullRequest> = None;
    // Published Pull Requests in local parent order, for registering stacks.
    let mut published_links: Vec<Published> = Vec::new();

    for (prepared_commit, pull_request) in zip(prepared_commits.iter_mut(), pull_requests) {
        if result.is_err() {
            break;
        }

        if !opts.dry_run {
            write_commit_title(prepared_commit)?;
        }

        // A change that is not directly on master is stacked on its parent's
        // Pull Request. In range mode that is the one we just published;
        // otherwise look it up.
        let stacked_on_parent = native
            && prepared_commit.parent_oid != master_base_oid
            && !wants_cherry_pick(&opts, &prepared_commit.message);
        if stacked_on_parent && parent.is_none() {
            match lookup_parent_pull_request(jj, gh, config, prepared_commit).await {
                Ok(found) => {
                    if let Some(number) = found.number {
                        published_links.push(Published {
                            number: Some(number),
                            head: found.branch.clone(),
                            head_oid: found.head_oid,
                            base: None,
                        });
                    }
                    parent = Some(found);
                }
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }

        // The further implementation of the diff command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        let outcome = diff_impl(
            &opts,
            &mut message_on_prompt,
            jj,
            gh,
            config,
            prepared_commit,
            master_base_oid,
            pull_request,
            native,
            if stacked_on_parent {
                parent.as_ref()
            } else {
                None
            },
        )
        .await;

        match outcome {
            Ok(published) => {
                parent = use_range_mode.then(|| ParentPullRequest {
                    number: published.number,
                    branch: published.head.clone(),
                    head_oid: published.head_oid,
                });
                published_links.push(published);
            }
            Err(error) => result = Err(error),
        }
    }

    if native && result.is_ok() {
        add_error(
            &mut result,
            register_native_stacks(&opts, gh, config, &published_links).await,
        );
    }

    // This updates the commit message in the local Jujutsu repository (if it was
    // changed by the implementation)
    if !opts.dry_run {
        add_error(
            &mut result,
            jj.rewrite_commit_messages(prepared_commits.as_mut_slice()),
        );
    }

    if opts.dry_run {
        let actions: Vec<_> = prepared_commits
            .iter()
            .enumerate()
            .filter(|(_, c)| c.dry_run_action.is_some())
            .collect();

        output(
            "\n📋",
            &format!(
                "Dry run complete. Would process {} change(s):\n",
                actions.len()
            ),
        )?;

        for (idx, pc) in &actions {
            let title = pc
                .message
                .get(&crate::message::MessageSection::Title)
                .map(|t| &t[..])
                .unwrap_or("");
            let pos = idx + 1;

            let (action_label, base, head, reviewers_list) = match &pc.dry_run_action {
                Some(crate::jj::DryRunAction::Create {
                    base,
                    head,
                    draft,
                    reviewers,
                    ..
                }) => {
                    let label = if *draft {
                        "CREATE (draft)".to_string()
                    } else {
                        "CREATE".to_string()
                    };
                    (label, base.as_str(), head.as_str(), reviewers.clone())
                }
                Some(crate::jj::DryRunAction::Update {
                    pr_number,
                    base,
                    head,
                    ..
                }) => {
                    let label = format!("UPDATE PR #{pr_number}");
                    (label, base.as_str(), head.as_str(), vec![])
                }
                None => continue,
            };

            output(
                &format!("  #{pos}"),
                &format!("{action_label}  {}  \"{title}\"", pc.short_id),
            )?;
            output("     ", &format!("head: {head}"))?;
            output("     ", &format!("base: {base}"))?;
            if !reviewers_list.is_empty() {
                output(
                    "     ",
                    &format!("reviewers: {}", reviewers_list.join(", ")),
                )?;
            }
            output("", "")?;
        }
    }

    result
}

/// Predict each selected Pull Request's base branch after this run, for
/// deciding which native stacks survive it.
fn plan_native_bases(
    opts: &DiffOptions,
    config: &crate::config::Config,
    commits: &[crate::jj::PreparedCommit],
    pull_requests: &[Option<PullRequest>],
    master_base_oid: Oid,
) -> Vec<crate::stacks::PlannedPullRequest> {
    commits
        .iter()
        .enumerate()
        .map(|(i, commit)| {
            let pull_request = pull_requests[i].as_ref();
            let expected_base = if commit.parent_oid == master_base_oid
                || wants_cherry_pick(opts, &commit.message)
            {
                Some(config.master_ref.branch_name().to_string())
            } else if i > 0 {
                pull_requests[i - 1]
                    .as_ref()
                    .map(|parent| parent.head.branch_name().to_string())
            } else {
                None
            };
            crate::stacks::PlannedPullRequest {
                number: pull_request.map(|pr| pr.number),
                current_base: pull_request.map(|pr| pr.base.branch_name().to_string()),
                expected_base,
            }
        })
        .collect()
}

/// Find the Pull Request of a change's parent, which a natively stacked Pull
/// Request targets when the parent is not part of this run.
async fn lookup_parent_pull_request(
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    commit: &crate::jj::PreparedCommit,
) -> Result<ParentPullRequest> {
    let parent_commit = jj.prepare_commit(config, commit.parent_oid)?;
    let number = parent_commit.pull_request_number.ok_or_else(|| {
        Error::new(formatdoc!(
            "The parent change {} has no Pull Request yet. A natively stacked \
             Pull Request targets its parent's Pull Request, so submit the \
             whole stack with `jj spr diff --all`.",
            parent_commit.short_id
        ))
    })?;
    let pull_request = gh.get_pull_request(number).await?;
    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(formatdoc!(
            "The parent change's Pull Request #{number} is closed. Rebase this \
             change before submitting it.",
        )));
    }
    Ok(ParentPullRequest {
        number: Some(number),
        branch: pull_request.head,
        head_oid: pull_request.head_oid,
    })
}

/// Register the published chains of Pull Requests as native GitHub stacks.
/// The Pull Requests are already correct at this point, so problems with the
/// stack itself are reported as warnings.
async fn register_native_stacks(
    opts: &DiffOptions,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    published: &[Published],
) -> Result<()> {
    use crate::stacks::{
        PublishedLink, StackAction, chain_segments, format_pr_list, plan_stack_updates,
        segment_numbers,
    };

    let links: Vec<PublishedLink> = published
        .iter()
        .map(|p| PublishedLink {
            number: p.number,
            base: p.base.clone(),
            head: p.head.branch_name().to_string(),
        })
        .collect();
    let segments = chain_segments(&links);
    if segments.is_empty() {
        return Ok(());
    }

    if opts.dry_run {
        for segment in segments {
            let labels: Vec<String> = segment
                .iter()
                .map(|l| l.number.map_or("new PR".to_string(), |n| format!("#{n}")))
                .collect();
            output(
                "🧱",
                &format!("Would register GitHub stack: {}", labels.join(" ← ")),
            )?;
        }
        return Ok(());
    }
    let segments = segment_numbers(&segments);

    let Some(stacks) = gh.list_stacks().await? else {
        output(
            "ℹ️ ",
            &format!(
                "Stacked pull requests are not enabled for {}/{}; the Pull Requests \
                 target each other but are not registered as a GitHub stack.",
                config.owner, config.repo
            ),
        )?;
        return Ok(());
    };

    for action in plan_stack_updates(&stacks, &segments) {
        match action {
            StackAction::Create(pull_requests) => match gh.create_stack(&pull_requests).await {
                Ok(stack) => output(
                    "🧱",
                    &format!(
                        "Created GitHub stack #{}: {}",
                        stack.number,
                        format_pr_list(&pull_requests)
                    ),
                )?,
                Err(error) => {
                    output("⚠️", "Creating the GitHub stack failed")?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            },
            StackAction::Add {
                stack,
                pull_requests,
            } => match gh.add_to_stack(stack, &pull_requests).await {
                Ok(_) => output(
                    "🧱",
                    &format!(
                        "Added {} to GitHub stack #{stack}",
                        format_pr_list(&pull_requests)
                    ),
                )?,
                Err(error) => {
                    output("⚠️", &format!("Adding to GitHub stack #{stack} failed"))?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            },
            StackAction::Warn(message) => output("⚠️", &message)?,
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn diff_impl(
    opts: &DiffOptions,
    message_on_prompt: &mut String,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    local_commit: &mut crate::jj::PreparedCommit,
    master_base_oid: Oid,
    pull_request: Option<PullRequest>,
    native: bool,
    native_parent: Option<&ParentPullRequest>,
) -> Result<Published> {
    // Parsed commit message of the local commit
    let message = &mut local_commit.message;

    let (effective_cherry_pick, marker_changed) =
        resolve_cherry_pick(opts.cherry_pick, opts.no_cherry_pick, message);
    if marker_changed {
        local_commit.message_changed = true;
    }

    // Check if the local commit is based directly on the master branch.
    let directly_based_on_master = local_commit.parent_oid == master_base_oid;

    // Determine the trees the Pull Request branch and the base branch should
    // have when we're done here.
    let (new_head_tree, new_base_tree) = if !effective_cherry_pick || directly_based_on_master {
        // Unless the user tells us to --cherry-pick, these should be the trees
        // of the current commit and its parent.
        // If the current commit is directly based on master (i.e.
        // directly_based_on_master is true), then we can do this here even when
        // the user tells us to --cherry-pick, because we would cherry pick the
        // current commit onto its parent, which gives us the same tree as the
        // current commit has, and the master base is the same as this commit's
        // parent.
        let head_tree = jj.get_tree_oid_for_commit(local_commit.oid)?;
        let base_tree = jj.get_tree_oid_for_commit(local_commit.parent_oid)?;

        (head_tree, base_tree)
    } else {
        // Cherry-pick the current commit onto master
        let index = jj.cherrypick(local_commit.oid, master_base_oid)?;

        if index.has_conflicts() {
            return Err(Error::new(formatdoc!(
                "This commit cannot be cherry-picked on {master}.",
                master = config.master_ref.branch_name(),
            )));
        }

        // This is the tree we are getting from cherrypicking the local commit
        // on master.
        let cherry_pick_tree = jj.write_index(index)?;
        let master_tree = jj.get_tree_oid_for_commit(master_base_oid)?;

        (cherry_pick_tree, master_tree)
    };

    if let Some(number) = local_commit.pull_request_number {
        output(
            "#️⃣ ",
            &format!(
                "Pull Request #{}: {}",
                number,
                config.pull_request_url(number)
            ),
        )?;
    }

    if local_commit.pull_request_number.is_none() || opts.update_message {
        validate_commit_message(message)?;
    }

    if let Some(ref pull_request) = pull_request {
        if pull_request.state == PullRequestState::Closed {
            return Err(Error::new(formatdoc!(
                "Pull request is closed. If you want to open a new one, \
                 remove the 'Pull Request' section from the commit message."
            )));
        }

        if !opts.update_message {
            let mut pull_request_updates: PullRequestUpdate = Default::default();
            pull_request_updates.update_message(pull_request, message);

            if !pull_request_updates.is_empty() {
                output(
                    "⚠️",
                    indoc!(
                        "The Pull Request's title/message differ from the \
                         local commit's message.
                         Use `spr diff --update-message` to overwrite the \
                         title and message on GitHub with the local message, \
                         or `spr amend` to go the other way (rewrite the local \
                         commit message with what is on GitHub)."
                    ),
                )?;
            }
        }
    }

    // Parse "Reviewers" section, if this is a new Pull Request
    let mut requested_reviewers = PullRequestRequestReviewers::default();

    if local_commit.pull_request_number.is_none()
        && let Some(reviewers) = message.get(&MessageSection::Reviewers)
    {
        let reviewers = parse_name_list(reviewers);
        let mut checked_reviewers = Vec::new();

        for reviewer in reviewers {
            // Teams are indicated with a leading #
            if let Some(slug) = reviewer.strip_prefix('#') {
                if let Ok(team) = GitHub::get_github_team((&config.owner).into(), slug.into()).await
                {
                    requested_reviewers
                        .team_reviewers
                        .push(team.slug.to_string());

                    checked_reviewers.push(reviewer);
                } else {
                    return Err(Error::new(format!(
                        "Reviewers field contains unknown team '{}'",
                        reviewer
                    )));
                }
            } else if let Ok(user) = GitHub::get_github_user(reviewer.clone()).await {
                requested_reviewers.reviewers.push(user.login);
                if let Some(name) = user.name {
                    checked_reviewers.push(format!(
                        "{} ({})",
                        reviewer.clone(),
                        remove_all_parens(&name)
                    ));
                } else {
                    checked_reviewers.push(reviewer);
                }
            } else {
                return Err(Error::new(format!(
                    "Reviewers field contains unknown user '{}'",
                    reviewer
                )));
            }
        }

        message.insert(MessageSection::Reviewers, checked_reviewers.join(", "));
        local_commit.message_changed = true;
    }

    // Get the name of the existing Pull Request branch, or constuct one if
    // there is none yet.

    let title = message
        .get(&MessageSection::Title)
        .map(|t| &t[..])
        .unwrap_or("");

    let pull_request_branch = match &pull_request {
        Some(pr) => pr.head.clone(),
        None => {
            config.new_github_branch(&config.get_new_branch_name(&jj.get_all_ref_names()?, title))
        }
    };

    // Get the tree ids of the current head of the Pull Request, as well as the
    // base, and the commit id of the master commit this PR is currently based
    // on.
    // If there is no pre-existing Pull Request, we fill in the equivalent
    // values.
    let (pr_head_oid, pr_head_tree, pr_base_oid, pr_base_tree, pr_master_base) =
        if let Some(pr) = &pull_request {
            let pr_head_tree = jj.get_tree_oid_for_commit(pr.head_oid)?;

            let current_master_oid = jj.resolve_reference(config.master_ref.local())?;
            // Use git for merge base calculation since jj doesn't expose this directly
            let pr_base_oid = jj.git_repo.merge_base(pr.head_oid, pr.base_oid)?;
            let pr_base_tree = jj.get_tree_oid_for_commit(pr_base_oid)?;

            let pr_master_base = jj.git_repo.merge_base(pr.head_oid, current_master_oid)?;

            (
                pr.head_oid,
                pr_head_tree,
                pr_base_oid,
                pr_base_tree,
                pr_master_base,
            )
        } else {
            let master_base_tree = jj.get_tree_oid_for_commit(master_base_oid)?;
            (
                master_base_oid,
                master_base_tree,
                master_base_oid,
                master_base_tree,
                master_base_oid,
            )
        };
    let needs_merging_master = pr_master_base != master_base_oid;

    // With native stacks, an existing Pull Request also needs an update when
    // it does not target (and contain) its parent's current Pull Request
    // branch, or targets anything but master when it has no parent PR.
    let base_is_expected = match (&pull_request, native, native_parent) {
        (Some(pr), true, Some(parent)) => {
            pr.base.branch_name() == parent.branch.branch_name()
                && is_ancestor(jj, parent.head_oid, pr.head_oid)?
        }
        (Some(pr), true, None) => pr.base.is_master_branch(),
        _ => true,
    };

    // At this point we can check if we can exit early because no update to the
    // existing Pull Request is necessary
    if let Some(ref pull_request) = pull_request {
        // So there is an existing Pull Request...
        if !needs_merging_master
            && base_is_expected
            && pr_head_tree == new_head_tree
            && pr_base_tree == new_base_tree
        {
            // ...and it does not need a rebase, and the trees of both Pull
            // Request branch and base are all the right ones.
            output("✅", "No update necessary")?;

            if opts.update_message {
                // However, the user requested to update the commit message on
                // GitHub

                let mut pull_request_updates: PullRequestUpdate = Default::default();
                pull_request_updates.update_message(pull_request, message);

                if !pull_request_updates.is_empty() {
                    if opts.dry_run {
                        output(
                            "  ",
                            &format!("Would update PR #{} title/body", pull_request.number),
                        )?;
                    } else {
                        // ...and there are actual changes to the message
                        gh.update_pull_request(pull_request.number, pull_request_updates)
                            .await?;
                        output("✍", "Updated commit message on GitHub")?;
                    }
                }
            }

            return Ok(Published {
                number: Some(pull_request.number),
                head: pull_request.head.clone(),
                head_oid: pull_request.head_oid,
                base: Some(pull_request.base.branch_name().to_string()),
            });
        }
    }

    // Check if there is a base branch on GitHub already. That's the case when
    // there is an existing Pull Request, and its base is not the master branch.
    let base_branch = if let Some(ref pr) = pull_request {
        if pr.base.is_master_branch() {
            None
        } else {
            Some(pr.base.clone())
        }
    } else {
        None
    };

    // We are going to construct `pr_base_parent: Option<Oid>`.
    // The value will be the commit we have to merge into the new Pull Request
    // commit to reflect changes in the parent of the local commit (by rebasing
    // or changing commits between master and this one, although technically
    // that's also rebasing).
    // If it's `None`, then we will not merge anything into the new Pull Request
    // commit.
    // If we are updating an existing PR, then there are three cases here:
    // (1) the parent tree of this commit is unchanged and we do not need to
    //     merge in master, which means that the local commit was amended, but
    //     not rebased. We don't need to merge anything into the Pull Request
    //     branch.
    // (2) the parent tree has changed, but the parent of the local commit is on
    //     master (or we are cherry-picking) and we are not already using a base
    //     branch: in this case we can merge the master commit we are based on
    //     into the PR branch, without going via a base branch. Thus, we don't
    //     introduce a base branch here and the PR continues to target the
    //     master branch.
    // (3) the parent tree has changed, and we need to use a base branch (either
    //     because one was already created earlier, or we find that we are not
    //     directly based on master now): we need to construct a new commit for
    //     the base branch. That new commit's tree is always that of that local
    //     commit's parent (thus making sure that the difference between base
    //     branch and pull request branch are exactly the changes made by the
    //     local commit, thus the changes we want to have reviewed). The new
    //     commit may have one or two parents. The previous base is always a
    //     parent (that's either the current commit on an existing base branch,
    //     or the previous master commit the PR was based on if there isn't a
    //     base branch already). In addition, if the master commit this commit
    //     is based on has changed, (i.e. the local commit got rebased on newer
    //     master in the meantime) then we have to merge in that master commit,
    //     which will be the second parent.
    // If we are creating a new pull request then `pr_base_tree` (the current
    // base of the PR) was set above to be the tree of the master commit the
    // local commit is based one, whereas `new_base_tree` is the tree of the
    // parent of the local commit. So if the local commit for this new PR is on
    // master, those two are the same (and we want to apply case 1). If the
    // commit is not directly based on master, we have to create this new PR
    // with a base branch, so that is case 3.

    let (pr_base_parent, base_branch) = if let Some(parent) = native_parent {
        // Native stack: the Pull Request targets the parent change's Pull
        // Request branch, and its new commit merges in that branch's head.
        // That keeps GitHub's diff (against the merge base) down to exactly
        // this change. No base branch of our own is created.
        if !opts.dry_run && jj.get_tree_oid_for_commit(parent.head_oid)? != new_base_tree {
            return Err(Error::new(formatdoc!(
                "The Pull Request for the parent change{} does not contain the                  parent change as it is locally (was it submitted with                  --cherry-pick, or not updated yet?). Run `jj spr diff --all`                  to update the whole stack.",
                parent
                    .number
                    .map(|n| format!(" (#{n})"))
                    .unwrap_or_default()
            )));
        }
        let merge_in_parent =
            pull_request.is_none() || !is_ancestor(jj, parent.head_oid, pr_head_oid)?;
        (
            merge_in_parent.then_some(parent.head_oid),
            Some(parent.branch.clone()),
        )
    } else if native {
        // Native stack, but no parent PR: this Pull Request targets master,
        // even if it used to target a parent's branch or one of jj-spr's
        // base branches.
        if pr_base_tree == new_base_tree && !needs_merging_master && base_is_expected {
            (None, None)
        } else {
            (Some(master_base_oid), None)
        }
    } else if pr_base_tree == new_base_tree && !needs_merging_master {
        // Case 1
        (None, base_branch)
    } else if base_branch.is_none() && (directly_based_on_master || effective_cherry_pick) {
        // Case 2
        (Some(master_base_oid), None)
    } else {
        // Case 3

        // We are constructing a base branch commit.
        // One parent of the new base branch commit will be the current base
        // commit, that could be either the top commit of an existing base
        // branch, or a commit on master.
        let mut parents = vec![pr_base_oid];

        // If we need to rebase on master, make the master commit also a
        // parent (except if the first parent is that same commit, we don't
        // want duplicates in `parents`).
        if needs_merging_master && pr_base_oid != master_base_oid {
            parents.push(master_base_oid);
        }

        let new_base_branch_commit = if opts.dry_run {
            // Use a placeholder OID — this won't be pushed
            pr_base_oid
        } else {
            jj.create_derived_commit(
                local_commit.parent_oid,
                &format!(
                    "[spr] {}\n\n[skip ci]",
                    if pull_request.is_some() {
                        "changes introduced through rebase".to_string()
                    } else {
                        format!(
                            "changes to {} this commit is based on",
                            config.master_ref.branch_name()
                        )
                    },
                ),
                new_base_tree,
                &parents[..],
            )?
        };

        // If `base_branch` is `None` (which means a base branch does not exist
        // yet), then make a `GitHubBranch` with a new name for a base branch
        let base_branch = if let Some(base_branch) = base_branch {
            base_branch
        } else {
            config.new_github_branch(&config.get_base_branch_name(&jj.get_all_ref_names()?, title))
        };

        (Some(new_base_branch_commit), Some(base_branch))
    };

    let mut github_commit_message = opts.message.clone();
    if pull_request.is_some() && github_commit_message.is_none() && !opts.dry_run {
        let input = {
            let message_on_prompt = message_on_prompt.clone();

            tokio::task::spawn_blocking(move || {
                dialoguer::Input::<String>::new()
                    .with_prompt("Message (leave empty to abort)")
                    .with_initial_text(message_on_prompt)
                    .allow_empty(true)
                    .interact_text()
            })
            .await??
        };

        if input.is_empty() {
            return Err(Error::new("Aborted as per user request".to_string()));
        }

        *message_on_prompt = input.clone();
        github_commit_message = Some(input);
    }

    // Construct the new commit for the Pull Request branch. First parent is the
    // current head commit of the Pull Request (we set this to the master base
    // commit earlier if the Pull Request does not yet exist)
    let mut pr_commit_parents = if pull_request.is_none() && native_parent.is_some() {
        // A new natively stacked Pull Request starts on top of its parent's
        // Pull Request branch.
        vec![]
    } else {
        vec![pr_head_oid]
    };

    // If we prepared a commit earlier that needs merging into the Pull Request
    // branch, then that commit is a parent of the new Pull Request commit.
    if let Some(oid) = pr_base_parent {
        // ...unless if that's the same commit as the one we added to
        // pr_commit_parents first.
        if pr_commit_parents.first() != Some(&oid) {
            pr_commit_parents.push(oid);
        }
    }

    // In a native stack, GitHub rebases the Pull Requests left after a stacked
    // merge by replaying their commits and dropping merge commits. An update
    // that merges in a new parent head and also carries local edits would
    // lose those edits there, so split it: a merge commit with only the
    // merged result, then an ordinary commit with the local change's tree.
    // (If the merge conflicts, keep the single merge commit.)
    if native
        && pull_request.is_some()
        && pr_commit_parents.len() == 2
        && !opts.dry_run
        && let Some(merged_tree) = jj.merge_trees(pr_base_tree, pr_head_tree, new_base_tree)?
        && merged_tree != new_head_tree
    {
        let merge_commit = jj.create_derived_commit(
            local_commit.oid,
            "[spr] changes introduced through rebase",
            merged_tree,
            &pr_commit_parents[..],
        )?;
        pr_commit_parents = vec![merge_commit];
    }

    let mut published_number = pull_request.as_ref().map(|pr| pr.number);

    // Create the new commit
    let pr_commit = if opts.dry_run {
        // Use a placeholder OID — this won't be pushed
        pr_head_oid
    } else {
        jj.create_derived_commit(
            local_commit.oid,
            github_commit_message
                .as_ref()
                .map(|s| &s[..])
                .unwrap_or_else(|| title),
            new_head_tree,
            &pr_commit_parents[..],
        )?
    };

    if opts.dry_run {
        let base_ref = base_branch.as_ref().unwrap_or(&config.master_ref);
        let base_branch_name = base_ref.branch_name();
        let head_branch_name = pull_request_branch.branch_name();
        let is_stacked = !base_ref.is_master_branch();

        local_commit.dry_run_action = if let Some(ref pr) = pull_request {
            Some(crate::jj::DryRunAction::Update {
                pr_number: pr.number,
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
            })
        } else {
            let all_reviewers: Vec<String> = requested_reviewers
                .reviewers
                .iter()
                .chain(requested_reviewers.team_reviewers.iter())
                .cloned()
                .collect();
            Some(crate::jj::DryRunAction::Create {
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
                draft: opts.draft,
                reviewers: all_reviewers,
            })
        };
    } else {
        let mut cmd = jj.git_command();
        cmd.arg("push")
            .arg("--atomic")
            .arg("--no-verify")
            .arg("--")
            .arg(&config.remote_name)
            .arg(format!("{}:{}", pr_commit, pull_request_branch.on_github()));

        if let Some(pull_request) = pull_request {
            // We are updating an existing Pull Request

            if needs_merging_master {
                output(
                    "⚾",
                    &format!(
                        "Commit was rebased - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            } else {
                output(
                    "🔁",
                    &format!(
                        "Commit was changed - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            }

            // Things we want to update in the Pull Request on GitHub
            let mut pull_request_updates: PullRequestUpdate = Default::default();

            if opts.update_message {
                pull_request_updates.update_message(&pull_request, message);
            }

            if let Some(ref base_branch) = base_branch {
                // We are using a base branch.

                if let Some(base_branch_commit) = pr_base_parent.filter(|_| native_parent.is_none())
                {
                    // ...and we prepared a new commit for it, so we need to push an
                    // update of the base branch. (A natively stacked PR's base is
                    // its parent's PR branch, which was pushed already.)
                    cmd.arg(format!(
                        "{}:{}",
                        base_branch_commit,
                        base_branch.on_github()
                    ));
                }

                // Push the new commit onto the Pull Request branch (and also the
                // new base commit, if we added that to cmd above).
                run_command(&mut cmd)
                    .await
                    .reword("git push failed".to_string())?;

                // If the Pull Request's base is not set to the base branch yet,
                // change that now.
                if pull_request.base.branch_name() != base_branch.branch_name() {
                    pull_request_updates.base = Some(base_branch.branch_name().to_string());
                }
            } else {
                // The Pull Request is against the master branch. In that case we
                // only need to push the update to the Pull Request branch.
                run_command(&mut cmd)
                    .await
                    .reword("git push failed".to_string())?;

                // A natively stacked Pull Request whose parent is gone (landed,
                // or abandoned locally) moves back to master.
                if !pull_request.base.is_master_branch() {
                    pull_request_updates.base = Some(config.master_ref.branch_name().to_string());
                }
            }

            if !pull_request_updates.is_empty() {
                let changes_base = pull_request_updates.base.is_some();
                let update = gh
                    .update_pull_request(pull_request.number, pull_request_updates)
                    .await;
                if native && changes_base {
                    update.context(
                        "GitHub does not allow changing the base of a Pull Request \
                         in a stack. Run `jj spr diff --all` from the top of the \
                         stack so jj-spr can rebuild it."
                            .to_string(),
                    )?;
                } else {
                    update?;
                }
            }
        } else {
            // We are creating a new Pull Request.

            // If there's a base branch, add it to the push (unless it is the
            // parent's Pull Request branch, which was pushed already)
            if let (Some(base_branch), Some(base_branch_commit), None) =
                (&base_branch, pr_base_parent, native_parent)
            {
                cmd.arg(format!(
                    "{}:{}",
                    base_branch_commit,
                    base_branch.on_github()
                ));
            }
            // Push the pull request branch and the base branch if present
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // Then call GitHub to create the Pull Request.
            let pull_request_number = gh
                .create_pull_request(
                    message,
                    base_branch
                        .as_ref()
                        .unwrap_or(&config.master_ref)
                        .branch_name()
                        .to_string(),
                    pull_request_branch.branch_name().to_string(),
                    opts.draft,
                )
                .await?;

            published_number = Some(pull_request_number);
            let pull_request_url = config.pull_request_url(pull_request_number);

            output(
                "✨",
                &format!(
                    "Created new Pull Request #{}: {}",
                    pull_request_number, pull_request_url,
                ),
            )?;

            message.insert(MessageSection::PullRequest, pull_request_url);
            local_commit.message_changed = true;

            let result = gh
                .request_reviewers(pull_request_number, requested_reviewers)
                .await;
            match result {
                Ok(()) => (),
                Err(error) => {
                    output("⚠️", "Requesting reviewers failed")?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            }
        }
    }

    Ok(Published {
        number: published_number,
        head: pull_request_branch,
        head_oid: pr_commit,
        base: Some(
            base_branch
                .as_ref()
                .unwrap_or(&config.master_ref)
                .branch_name()
                .to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config() -> crate::config::Config {
        crate::config::Config::new(
            "test_owner".into(),
            "test_repo".into(),
            "origin".into(),
            "main".into(),
            "spr/test/".into(),
            false,
        )
    }

    #[allow(dead_code)]
    fn create_test_git_repo() -> (TempDir, git2::Repository) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let repo = git2::Repository::init(temp_dir.path()).expect("Failed to init git repo");

        // Create initial commit
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");
        let tree_id = {
            let mut index = repo.index().expect("Failed to get index");
            index.write_tree().expect("Failed to write tree")
        };
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Initial commit",
            &tree,
            &[],
        )
        .expect("Failed to create initial commit");

        drop(tree); // Drop the tree reference before moving repo
        (temp_dir, repo)
    }

    #[allow(dead_code)]
    fn create_test_commit(repo: &git2::Repository, message: &str, content: &str) -> git2::Oid {
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");

        // Write content to a test file
        let repo_path = repo.workdir().expect("Failed to get workdir");
        let file_path = repo_path.join("test.txt");
        fs::write(&file_path, content).expect("Failed to write test file");

        // Add file to index
        let mut index = repo.index().expect("Failed to get index");
        index
            .add_path(std::path::Path::new("test.txt"))
            .expect("Failed to add file to index");
        index.write().expect("Failed to write index");

        let tree_id = index.write_tree().expect("Failed to write tree");
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        // Get HEAD commit as parent
        let parent_commit = repo
            .head()
            .expect("Failed to get HEAD")
            .peel_to_commit()
            .expect("Failed to peel to commit");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )
        .expect("Failed to create commit")
    }

    #[test]
    fn test_diff_options_default_values() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert!(!opts.all);
        assert!(!opts.update_message);
        assert!(!opts.draft);
        assert!(!opts.cherry_pick);
        assert!(opts.message.is_none());
        assert!(opts.base.is_none());
    }

    #[test]
    fn test_diff_options_with_base() {
        let opts = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert_eq!(opts.base, Some("main".to_string()));
        assert!(opts.all);
    }

    #[test]
    fn test_jujutsu_integration() {
        // Test configuration for jj-spr
        let config = create_test_config();
        assert_eq!(config.owner, "test_owner");
        assert_eq!(config.remote_name, "origin");
    }

    #[test]
    fn test_base_option_parsing() {
        // Test that the base option can be parsed correctly
        let opts_with_base = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert_eq!(opts_with_base.base.as_deref(), Some("main"));
        assert!(opts_with_base.all);

        let opts_with_trunk = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert_eq!(opts_with_trunk.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_all_flag_behavior() {
        let opts_with_all = DiffOptions {
            all: true,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        // When --all is specified, it should work with base revisions
        assert!(opts_with_all.all);
        assert!(opts_with_all.base.is_some());
    }

    #[test]
    fn test_diff_options_combinations() {
        // Test various valid combinations of options
        let opts = DiffOptions {
            all: true,
            update_message: true,
            draft: true,
            message: Some("Update message".to_string()),
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert!(opts.all);
        assert!(opts.update_message);
        assert!(opts.draft);
        assert_eq!(opts.message.as_deref(), Some("Update message"));
        assert!(!opts.cherry_pick);
        assert_eq!(opts.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_diff_options_dry_run_flag() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: true,
            native_stack: false,
            no_native_stack: false,
            discard_remote_changes: false,
        };

        assert!(opts.dry_run);
        assert!(!opts.all);
    }

    // -------------------------------------------------------------------------
    // resolve_cherry_pick unit tests

    fn make_map_with_cherry_pick(value: &str) -> crate::message::MessageSectionsMap {
        [(
            crate::message::MessageSection::CherryPick,
            value.to_string(),
        )]
        .into()
    }

    #[test]
    fn test_resolve_cherry_pick_default_no_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(!effective);
        assert!(!changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_default_with_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_case_insensitive() {
        let mut map = make_map_with_cherry_pick("TRUE");
        let (effective, _) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_other_value() {
        for value in &["false", "yes", "1", ""] {
            let mut map = make_map_with_cherry_pick(value);
            let (effective, _) = resolve_cherry_pick(false, false, &mut map);
            assert!(!effective, "Expected false for marker value {:?}", value);
        }
    }

    #[test]
    fn test_resolve_cherry_pick_flag_inserts_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(changed);
        assert_eq!(
            map.get(&crate::message::MessageSection::CherryPick)
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_resolve_cherry_pick_flag_idempotent() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_removes_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_when_absent() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(!changed);
    }

    #[test]
    fn test_diff_options_parse_no_cherry_pick() {
        use clap::Parser;
        let opts = DiffOptions::try_parse_from(["test", "--no-cherry-pick"]).unwrap();
        assert!(opts.no_cherry_pick);
        assert!(!opts.cherry_pick);
    }

    #[test]
    fn test_diff_options_cherry_pick_no_cherry_pick_conflict() {
        use clap::Parser;
        let result = DiffOptions::try_parse_from(["test", "--cherry-pick", "--no-cherry-pick"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    // Integration tests would require more complex setup with actual Git repositories
    // and proper mocking of GitHub API calls. The tests above focus on:
    // 1. Option parsing and validation
    // 2. Data structure correctness
    // 3. Basic logic flow verification
    //
    // For full integration testing, consider:
    // - Mocking GitHub API responses
    // - Creating test repositories with specific commit structures
    // - Testing the interaction between revision specification and commit preparation
}
