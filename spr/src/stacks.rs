/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Planning for native GitHub stacks.
//!
//! GitHub's Stacks API registers a chain of Pull Requests in which every PR's
//! base branch is the head branch of the PR below it. The functions here
//! decide, without talking to GitHub, which stacks have to be dissolved
//! before `jj spr diff` retargets Pull Requests (GitHub refuses to change the
//! base of a stacked Pull Request) and which stacks to create or extend
//! afterwards. Keeping this logic free of I/O lets it be unit tested.

use crate::github::{GitHubStack, ReviewDecision};

/// One Pull Request in the selection, in local parent order (bottom first),
/// as it is about to be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPullRequest {
    /// The Pull Request number, if the PR exists already.
    pub number: Option<u64>,
    /// The base branch the PR has on GitHub right now (existing PRs only).
    pub current_base: Option<String>,
    /// The base branch the PR will have after this run. `None` means the
    /// base is a PR branch that does not exist yet.
    pub expected_base: Option<String>,
}

/// Decide which existing stacks must be removed before the selection is
/// published.
///
/// A stack survives when every one of its members in the selection keeps its
/// base branch. Otherwise it has to be dissolved first, which is only safe
/// when all of its open members are in the selection (they are all about to
/// be republished, and the stack is rebuilt afterwards). Returns the stack
/// numbers to dissolve, or an explanation of why the run must stop before
/// changing anything.
pub fn plan_unstacks(
    stacks: &[GitHubStack],
    selection: &[PlannedPullRequest],
) -> std::result::Result<Vec<u64>, String> {
    let selected: Vec<u64> = selection.iter().filter_map(|pr| pr.number).collect();
    let mut to_dissolve = Vec::new();

    for stack in stacks {
        let active = stack.active_pull_requests();
        let members_in_selection: Vec<&PlannedPullRequest> = selection
            .iter()
            .filter(|pr| pr.number.is_some_and(|n| active.contains(&n)))
            .collect();
        if members_in_selection.is_empty() {
            continue;
        }

        let bases_unchanged = members_in_selection.iter().all(|pr| {
            pr.expected_base.is_some() && pr.current_base.as_ref() == pr.expected_base.as_ref()
        });
        let selection_order: Vec<u64> = members_in_selection
            .iter()
            .filter_map(|pr| pr.number)
            .collect();
        let same_order = is_contiguous_run(&active, &selection_order);

        if bases_unchanged && same_order {
            continue;
        }

        let outside: Vec<u64> = active
            .iter()
            .copied()
            .filter(|n| !selected.contains(n))
            .collect();
        if !outside.is_empty() {
            return Err(format!(
                "GitHub stack #{} has to be rebuilt, but it also contains {} that \
                 this run would not update. Run `jj spr diff --all` from the top \
                 of the stack so every member is republished.",
                stack.number,
                format_pr_list(&outside),
            ));
        }
        to_dissolve.push(stack.number);
    }

    Ok(to_dissolve)
}

/// Split published Pull Requests into chains. A PR continues the chain of the
/// PR before it when its base branch is that PR's head branch.
pub fn chain_segments(published: &[PublishedLink]) -> Vec<Vec<&PublishedLink>> {
    let mut segments: Vec<Vec<&PublishedLink>> = Vec::new();
    let mut previous: Option<&PublishedLink> = None;

    for link in published {
        let continues = previous.is_some_and(|prev| link.base.as_deref() == Some(&prev.head));
        match (continues, segments.last_mut()) {
            (true, Some(segment)) => segment.push(link),
            _ => segments.push(vec![link]),
        }
        previous = Some(link);
    }

    segments.into_iter().filter(|s| s.len() >= 2).collect()
}

/// The numbers of the Pull Requests in each chain, for the Stacks API.
pub fn segment_numbers(segments: &[Vec<&PublishedLink>]) -> Vec<Vec<u64>> {
    segments
        .iter()
        .map(|segment| segment.iter().filter_map(|link| link.number).collect())
        .collect()
}

/// A Pull Request after it was published: its number (`None` until it is
/// created, in a dry run) and branches. A `base` of `None` (not known, for
/// example a parent Pull Request looked up outside this run) never continues
/// a chain, but the link can start one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedLink {
    pub number: Option<u64>,
    pub base: Option<String>,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackAction {
    Create(Vec<u64>),
    Add { stack: u64, pull_requests: Vec<u64> },
    Warn(String),
}

/// Decide how to make GitHub's stacks match the published chains.
pub fn plan_stack_updates(stacks: &[GitHubStack], segments: &[Vec<u64>]) -> Vec<StackAction> {
    let mut actions = Vec::new();

    for segment in segments {
        let touching: Vec<&GitHubStack> = stacks
            .iter()
            .filter(|stack| {
                let active = stack.active_pull_requests();
                segment.iter().any(|n| active.contains(n))
            })
            .collect();

        match touching.as_slice() {
            [] => actions.push(StackAction::Create(segment.clone())),
            [stack] => {
                let active = stack.active_pull_requests();
                if is_contiguous_run(&active, segment) {
                    // Already registered as it is.
                    continue;
                }
                // The stack's top overlaps the start of the chain: append
                // the rest.
                let overlap = (1..=active.len().min(segment.len()))
                    .rev()
                    .find(|&k| active[active.len() - k..] == segment[..k]);
                // Only PRs that are in no stack yet can be appended.
                let overlap = overlap.filter(|&k| segment[k..].iter().all(|n| !active.contains(n)));
                let inserted_below_top = active.contains(&segment[0])
                    && active.last() != Some(&segment[0])
                    && segment[1..].iter().all(|n| !active.contains(n));
                match overlap {
                    Some(k) => actions.push(StackAction::Add {
                        stack: stack.number,
                        pull_requests: segment[k..].to_vec(),
                    }),
                    None if inserted_below_top => actions.push(StackAction::Warn(format!(
                        "{} now sit{} on #{}, in the middle of GitHub stack #{} ({}). \
                         GitHub can only append to a stack, so run `jj spr diff --all` \
                         from the top of the stack to rebuild it with the new order.",
                        format_pr_list(&segment[1..]),
                        if segment.len() == 2 { "s" } else { "" },
                        segment[0],
                        stack.number,
                        format_pr_list(&active),
                    ))),
                    None => actions.push(StackAction::Warn(format!(
                        "GitHub stack #{} ({}) does not match the local order ({}). \
                         Run `jj spr diff --all` from the top of the stack to rebuild it.",
                        stack.number,
                        format_pr_list(&active),
                        format_pr_list(segment),
                    ))),
                }
            }
            several => actions.push(StackAction::Warn(format!(
                "Pull Requests {} belong to several GitHub stacks ({}); leaving them \
                 unchanged.",
                format_pr_list(segment),
                several
                    .iter()
                    .map(|s| format!("#{}", s.number))
                    .collect::<Vec<_>>()
                    .join(", "),
            ))),
        }
    }

    actions
}

/// Whether `needle` appears in `haystack` as one contiguous run.
fn is_contiguous_run(haystack: &[u64], needle: &[u64]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

pub fn format_pr_list(numbers: &[u64]) -> String {
    numbers
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A local change as `jj spr stack` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedChange {
    /// Short change ID and title, for messages.
    pub label: String,
    pub pr: Option<ReviewedPullRequest>,
    /// Submitted with `--cherry-pick`, so it targets master on its own.
    pub cherry_pick: bool,
    /// The local change differs from the Pull Request's current head.
    pub differs_from_pr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedPullRequest {
    pub number: u64,
    pub state: ReviewedState,
    pub draft: bool,
    pub base: String,
    pub head: String,
    pub review: ReviewDecision,
    /// Names of the CI checks on the head that failed, and that have not
    /// finished yet.
    pub failing_checks: Vec<String>,
    pub pending_checks: Vec<String>,
    pub unresolved_threads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewedState {
    Open,
    Merged,
    Closed,
}

/// Something about a stack that does not match GitHub, and what fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    NotSubmitted {
        change: String,
    },
    OutOfDate {
        number: u64,
    },
    Draft {
        number: u64,
    },
    FailingChecks {
        number: u64,
        checks: Vec<String>,
    },
    ChangesRequested {
        number: u64,
    },
    UnresolvedThreads {
        number: u64,
        count: usize,
    },
    Merged {
        number: u64,
    },
    Closed {
        number: u64,
    },
    /// `below` is the Pull Request of the change below, if it has one;
    /// otherwise the Pull Request should target master (`expected`).
    WrongBase {
        number: u64,
        actual: String,
        expected: String,
        below: Option<u64>,
    },
    Stack(StackAction),
    StacksUnavailable,
}

impl Finding {
    /// A stable name for the kind of finding, for `jj spr stack --json`.
    pub fn kind(&self) -> &'static str {
        match self {
            Finding::NotSubmitted { .. } => "not_submitted",
            Finding::OutOfDate { .. } => "needs_update",
            Finding::Draft { .. } => "draft",
            Finding::FailingChecks { .. } => "failing_checks",
            Finding::ChangesRequested { .. } => "changes_requested",
            Finding::UnresolvedThreads { .. } => "unresolved_threads",
            Finding::Merged { .. } => "merged",
            Finding::Closed { .. } => "closed",
            Finding::WrongBase { .. } => "wrong_base",
            Finding::Stack(StackAction::Create(_)) => "stack_missing",
            Finding::Stack(StackAction::Add { .. }) => "stack_incomplete",
            Finding::Stack(StackAction::Warn(_)) => "stack_mismatch",
            Finding::StacksUnavailable => "stacks_unavailable",
        }
    }

    /// The Pull Request the finding is about, if it is about one.
    pub fn pull_request(&self) -> Option<u64> {
        match self {
            Finding::OutOfDate { number }
            | Finding::Draft { number }
            | Finding::FailingChecks { number, .. }
            | Finding::ChangesRequested { number }
            | Finding::UnresolvedThreads { number, .. }
            | Finding::Merged { number }
            | Finding::Closed { number }
            | Finding::WrongBase { number, .. } => Some(*number),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Finding::NotSubmitted { change } => {
                format!("{change} has no Pull Request yet. Run `jj spr diff --all`.")
            }
            Finding::OutOfDate { number } => format!(
                "The local change for #{number} differs from the Pull Request. Run \
                 `jj spr diff --all` to update it."
            ),
            Finding::Draft { number } => format!(
                "#{number} is a draft. Mark it ready for review on GitHub before landing it."
            ),
            Finding::FailingChecks { number, checks } => {
                format!("#{number} has failing checks: {}.", checks.join(", "))
            }
            Finding::ChangesRequested { number } => {
                format!("#{number} has changes requested.")
            }
            Finding::UnresolvedThreads { number, count } => format!(
                "#{number} has {count} unresolved review thread{}.",
                if *count == 1 { "" } else { "s" }
            ),
            Finding::Merged { number } => format!(
                "#{number} has merged. Run `jj spr sync` to abandon it locally and \
                 rebase the rest of the stack."
            ),
            Finding::Closed { number } => format!(
                "#{number} was closed without merging. Abandon the change, or remove \
                 its `Pull Request:` line to open a new one."
            ),
            Finding::WrongBase {
                number,
                actual,
                expected,
                below: Some(below),
            } => format!(
                "#{number} targets {actual}, but the change below it is #{below} \
                 ({expected}). Run `jj spr diff --all` to restack it."
            ),
            Finding::WrongBase {
                number,
                actual,
                expected,
                below: None,
            } => format!(
                "#{number} targets {actual}, but it should target {expected}. Run \
                 `jj spr diff --all` to restack it."
            ),
            Finding::Stack(StackAction::Create(prs)) => format!(
                "{} target each other but are not registered as a GitHub stack. Run \
                 `jj spr diff --all`.",
                format_pr_list(prs)
            ),
            Finding::Stack(StackAction::Add {
                stack,
                pull_requests,
            }) => format!(
                "{} {} not in GitHub stack #{stack} yet. Run `jj spr diff --all`.",
                format_pr_list(pull_requests),
                if pull_requests.len() == 1 {
                    "is"
                } else {
                    "are"
                }
            ),
            Finding::Stack(StackAction::Warn(message)) => message.clone(),
            Finding::StacksUnavailable => "Stacked pull requests are not enabled for this \
                                           repository, so the Pull Requests are chained but \
                                           not shown as a stack on GitHub."
                .to_string(),
        }
    }
}

/// Compare a local stack (bottom first) with its Pull Requests and GitHub's
/// stacks. `stacks` is `None` when stacked pull requests are not enabled.
/// Base branches and stack membership are only checked for native stacks.
pub fn review_stack(
    changes: &[ReviewedChange],
    stacks: Option<&[GitHubStack]>,
    native: bool,
    master: &str,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for (i, change) in changes.iter().enumerate() {
        let Some(pr) = &change.pr else {
            findings.push(Finding::NotSubmitted {
                change: change.label.clone(),
            });
            continue;
        };
        match pr.state {
            ReviewedState::Merged => {
                findings.push(Finding::Merged { number: pr.number });
                continue;
            }
            ReviewedState::Closed => {
                findings.push(Finding::Closed { number: pr.number });
                continue;
            }
            ReviewedState::Open => {}
        }
        if change.differs_from_pr {
            findings.push(Finding::OutOfDate { number: pr.number });
        }
        if pr.draft {
            findings.push(Finding::Draft { number: pr.number });
        }
        if !pr.failing_checks.is_empty() {
            findings.push(Finding::FailingChecks {
                number: pr.number,
                checks: pr.failing_checks.clone(),
            });
        }
        if pr.review == ReviewDecision::ChangesRequested {
            findings.push(Finding::ChangesRequested { number: pr.number });
        }
        if pr.unresolved_threads > 0 {
            findings.push(Finding::UnresolvedThreads {
                number: pr.number,
                count: pr.unresolved_threads,
            });
        }
        if native {
            // The first change sits on master, as does a cherry-pick; any other
            // change targets the Pull Request of the change below it.
            let expected = if i == 0 || change.cherry_pick {
                Some((master.to_string(), None))
            } else {
                match &changes[i - 1].pr {
                    Some(below) if below.state == ReviewedState::Open => {
                        Some((below.head.clone(), Some(below.number)))
                    }
                    _ => None, // reported for the change below
                }
            };
            if let Some((expected, below)) = expected
                && pr.base != expected
            {
                findings.push(Finding::WrongBase {
                    number: pr.number,
                    actual: pr.base.clone(),
                    expected,
                    below,
                });
            }
        }
    }

    if native {
        let links: Vec<PublishedLink> = changes
            .iter()
            .filter_map(|c| c.pr.as_ref())
            .filter(|pr| pr.state == ReviewedState::Open)
            .map(|pr| PublishedLink {
                number: Some(pr.number),
                base: Some(pr.base.clone()),
                head: pr.head.clone(),
            })
            .collect();
        let segments = segment_numbers(&chain_segments(&links));
        match stacks {
            None if !segments.is_empty() => findings.push(Finding::StacksUnavailable),
            None => {}
            Some(stacks) => findings.extend(
                plan_stack_updates(stacks, &segments)
                    .into_iter()
                    .map(Finding::Stack),
            ),
        }
    }

    findings
}

/// How many changes, from the bottom of the stack, are ready to land:
/// submitted and up to date, open, not drafts, with passing (or no) checks,
/// no unresolved threads, and approved (or not needing approval). Without a
/// native stack only the bottom change can be landed, so this is at most
/// one.
pub fn ready_to_land(changes: &[ReviewedChange], native: bool, require_approval: bool) -> usize {
    let ready = changes
        .iter()
        .take_while(|change| {
            let Some(pr) = &change.pr else {
                return false;
            };
            let approved = match pr.review {
                ReviewDecision::Approved => true,
                ReviewDecision::None => !require_approval,
                ReviewDecision::ChangesRequested | ReviewDecision::Required => false,
            };
            pr.state == ReviewedState::Open
                && !change.differs_from_pr
                && !pr.draft
                && pr.failing_checks.is_empty()
                && pr.pending_checks.is_empty()
                && pr.unresolved_threads == 0
                && approved
        })
        .count();
    if native { ready } else { ready.min(1) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GitHubStackMember;

    fn stack(number: u64, members: &[(u64, bool)]) -> GitHubStack {
        GitHubStack {
            number,
            pull_requests: members
                .iter()
                .map(|&(n, merged)| GitHubStackMember {
                    number: n,
                    state: Some(if merged { "closed" } else { "open" }.to_string()),
                    merged_at: merged.then(|| "2026-01-01T00:00:00Z".to_string()),
                })
                .collect(),
        }
    }

    fn planned(
        number: Option<u64>,
        current: Option<&str>,
        expected: Option<&str>,
    ) -> PlannedPullRequest {
        PlannedPullRequest {
            number,
            current_base: current.map(str::to_string),
            expected_base: expected.map(str::to_string),
        }
    }

    fn link(number: u64, base: &str, head: &str) -> PublishedLink {
        PublishedLink {
            number: Some(number),
            base: Some(base.to_string()),
            head: head.to_string(),
        }
    }

    fn reviewed(number: u64, base: &str, head: &str) -> ReviewedChange {
        ReviewedChange {
            label: format!("change{number} Title"),
            pr: Some(ReviewedPullRequest {
                number,
                state: ReviewedState::Open,
                draft: false,
                base: base.to_string(),
                head: head.to_string(),
                review: ReviewDecision::Approved,
                failing_checks: vec![],
                pending_checks: vec![],
                unresolved_threads: 0,
            }),
            cherry_pick: false,
            differs_from_pr: false,
        }
    }

    fn native_stack() -> Vec<ReviewedChange> {
        vec![
            reviewed(1, "main", "spr/a"),
            reviewed(2, "spr/a", "spr/b"),
            reviewed(3, "spr/b", "spr/c"),
        ]
    }

    #[test]
    fn review_clean_stack() {
        let stacks = [stack(1, &[(1, false), (2, false), (3, false)])];
        assert!(review_stack(&native_stack(), Some(&stacks), true, "main").is_empty());
    }

    #[test]
    fn review_reports_unregistered_stack() {
        assert_eq!(
            review_stack(&native_stack(), Some(&[]), true, "main"),
            vec![Finding::Stack(StackAction::Create(vec![1, 2, 3]))]
        );
    }

    #[test]
    fn review_reports_missing_member() {
        let stacks = [stack(1, &[(1, false), (2, false)])];
        assert_eq!(
            review_stack(&native_stack(), Some(&stacks), true, "main"),
            vec![Finding::Stack(StackAction::Add {
                stack: 1,
                pull_requests: vec![3]
            })]
        );
    }

    #[test]
    fn review_reports_stacks_unavailable() {
        assert_eq!(
            review_stack(&native_stack(), None, true, "main"),
            vec![Finding::StacksUnavailable]
        );
    }

    #[test]
    fn review_reports_wrong_base() {
        let mut changes = native_stack();
        changes[2].pr.as_mut().unwrap().base = "spr/main.c".into();
        let stacks = [stack(1, &[(1, false), (2, false)])];
        let findings = review_stack(&changes, Some(&stacks), true, "main");
        assert!(findings.contains(&Finding::WrongBase {
            number: 3,
            actual: "spr/main.c".into(),
            expected: "spr/b".into(),
            below: Some(2),
        }));
    }

    #[test]
    fn review_cherry_pick_targets_master() {
        let mut changes = vec![reviewed(1, "main", "spr/a"), reviewed(2, "main", "spr/b")];
        changes[1].cherry_pick = true;
        assert!(review_stack(&changes, Some(&[]), true, "main").is_empty());
    }

    #[test]
    fn review_per_pull_request_findings() {
        let mut changes = native_stack();
        changes[0].pr.as_mut().unwrap().state = ReviewedState::Merged;
        changes[1].pr.as_mut().unwrap().draft = true;
        changes[2].differs_from_pr = true;
        changes.push(ReviewedChange {
            label: "change4 New".into(),
            pr: None,
            cherry_pick: false,
            differs_from_pr: false,
        });
        let stacks = [stack(1, &[(1, true), (2, false), (3, false)])];
        let findings = review_stack(&changes, Some(&stacks), true, "main");
        assert_eq!(
            findings,
            vec![
                Finding::Merged { number: 1 },
                Finding::Draft { number: 2 },
                Finding::OutOfDate { number: 3 },
                Finding::NotSubmitted {
                    change: "change4 New".into()
                },
            ]
        );
    }

    #[test]
    fn review_legacy_stack_skips_base_and_stack_checks() {
        let changes = vec![
            reviewed(1, "main", "spr/a"),
            reviewed(2, "spr/main.b", "spr/b"),
        ];
        assert!(review_stack(&changes, Some(&[]), false, "main").is_empty());
    }

    #[test]
    fn unchanged_stack_is_kept() {
        let stacks = [stack(7, &[(1, false), (2, false)])];
        let selection = [
            planned(Some(1), Some("main"), Some("main")),
            planned(Some(2), Some("spr/a"), Some("spr/a")),
            planned(None, None, Some("spr/b")),
        ];
        assert_eq!(plan_unstacks(&stacks, &selection), Ok(vec![]));
    }

    #[test]
    fn retargeted_member_dissolves_stack() {
        // #2 moved below a new change, so its base changes.
        let stacks = [stack(7, &[(1, false), (2, false)])];
        let selection = [
            planned(Some(1), Some("main"), Some("main")),
            planned(None, None, Some("spr/a")),
            planned(Some(2), Some("spr/a"), None),
        ];
        assert_eq!(plan_unstacks(&stacks, &selection), Ok(vec![7]));
    }

    #[test]
    fn reordered_members_dissolve_stack() {
        let stacks = [stack(3, &[(1, false), (2, false)])];
        let selection = [
            planned(Some(2), Some("spr/a"), Some("main")),
            planned(Some(1), Some("main"), Some("spr/b")),
        ];
        assert_eq!(plan_unstacks(&stacks, &selection), Ok(vec![3]));
    }

    #[test]
    fn migration_from_synthetic_base_dissolves_nothing() {
        // PRs created by legacy jj-spr are not in any stack.
        let selection = [
            planned(Some(1), Some("main"), Some("main")),
            planned(Some(2), Some("spr/main.b"), Some("spr/a")),
        ];
        assert_eq!(plan_unstacks(&[], &selection), Ok(vec![]));
    }

    #[test]
    fn refuses_to_truncate_stack_with_unselected_members() {
        let stacks = [stack(4, &[(1, false), (2, false), (3, false)])];
        let selection = [
            planned(Some(1), Some("main"), Some("main")),
            planned(Some(2), Some("spr/a"), Some("main")),
        ];
        let error = plan_unstacks(&stacks, &selection).unwrap_err();
        assert!(error.contains("#3"), "{error}");
    }

    #[test]
    fn merged_members_do_not_block_rebuild() {
        let stacks = [stack(4, &[(1, true), (2, false), (3, false)])];
        let selection = [
            planned(Some(3), Some("spr/b"), Some("main")),
            planned(Some(2), Some("main"), Some("spr/c")),
        ];
        assert_eq!(plan_unstacks(&stacks, &selection), Ok(vec![4]));
    }

    #[test]
    fn segments_follow_base_chain() {
        let published = [
            link(1, "main", "spr/a"),
            link(2, "spr/a", "spr/b"),
            link(3, "main", "spr/c"), // cherry-picked: starts a new chain
            link(4, "spr/c", "spr/d"),
            link(5, "spr/d", "spr/e"),
        ];
        assert_eq!(
            segment_numbers(&chain_segments(&published)),
            vec![vec![1, 2], vec![3, 4, 5]]
        );
    }

    #[test]
    fn single_pull_requests_are_not_stacks() {
        let published = [link(1, "main", "spr/a"), link(2, "main", "spr/b")];
        assert!(chain_segments(&published).is_empty());
    }

    #[test]
    fn creates_missing_stack() {
        assert_eq!(
            plan_stack_updates(&[], &[vec![1, 2, 3]]),
            vec![StackAction::Create(vec![1, 2, 3])]
        );
    }

    #[test]
    fn existing_stack_needs_nothing() {
        let stacks = [stack(9, &[(1, false), (2, false)])];
        assert!(plan_stack_updates(&stacks, &[vec![1, 2]]).is_empty());
    }

    #[test]
    fn partial_selection_inside_stack_needs_nothing() {
        let stacks = [stack(9, &[(1, false), (2, false), (3, false)])];
        assert!(plan_stack_updates(&stacks, &[vec![2, 3]]).is_empty());
    }

    #[test]
    fn appends_new_top() {
        let stacks = [stack(9, &[(1, false), (2, false)])];
        assert_eq!(
            plan_stack_updates(&stacks, &[vec![1, 2, 3, 4]]),
            vec![StackAction::Add {
                stack: 9,
                pull_requests: vec![3, 4]
            }]
        );
    }

    #[test]
    fn appends_when_parent_is_top_of_stack() {
        // Single-change mode: the chain is [parent, this].
        let stacks = [stack(9, &[(1, false), (2, false)])];
        assert_eq!(
            plan_stack_updates(&stacks, &[vec![2, 3]]),
            vec![StackAction::Add {
                stack: 9,
                pull_requests: vec![3]
            }]
        );
    }

    #[test]
    fn ignores_merged_history_when_appending() {
        let stacks = [stack(9, &[(1, true), (2, false)])];
        assert_eq!(
            plan_stack_updates(&stacks, &[vec![2, 3]]),
            vec![StackAction::Add {
                stack: 9,
                pull_requests: vec![3]
            }]
        );
    }

    #[test]
    fn explains_insertion_inside_stack() {
        let stacks = [stack(9, &[(1, false), (2, false), (3, false)])];
        let actions = plan_stack_updates(&stacks, &[vec![2, 4]]);
        match actions.as_slice() {
            [StackAction::Warn(message)] => {
                assert!(message.contains("#4 now sits on #2"), "{message}");
                assert!(message.contains("jj spr diff --all"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn warns_on_mismatched_order() {
        let stacks = [stack(9, &[(2, false), (1, false)])];
        let actions = plan_stack_updates(&stacks, &[vec![1, 2]]);
        assert!(matches!(actions.as_slice(), [StackAction::Warn(_)]));
    }

    fn pr_mut(change: &mut ReviewedChange) -> &mut ReviewedPullRequest {
        change.pr.as_mut().unwrap()
    }

    #[test]
    fn review_reports_checks_reviews_and_threads() {
        let mut changes = native_stack();
        pr_mut(&mut changes[0]).failing_checks = vec!["build".into(), "clippy".into()];
        pr_mut(&mut changes[1]).review = ReviewDecision::ChangesRequested;
        pr_mut(&mut changes[2]).unresolved_threads = 2;
        let findings = review_stack(
            &changes,
            Some(&[stack(1, &[(1, false), (2, false), (3, false)])]),
            true,
            "main",
        );
        assert_eq!(
            findings,
            vec![
                Finding::FailingChecks {
                    number: 1,
                    checks: vec!["build".into(), "clippy".into()]
                },
                Finding::ChangesRequested { number: 2 },
                Finding::UnresolvedThreads {
                    number: 3,
                    count: 2
                },
            ]
        );
        assert_eq!(
            findings[0].message(),
            "#1 has failing checks: build, clippy."
        );
        assert_eq!(findings[2].message(), "#3 has 2 unresolved review threads.");
    }

    #[test]
    fn ready_to_land_counts_from_the_bottom() {
        let mut changes = native_stack();
        assert_eq!(ready_to_land(&changes, true, true), 3);
        assert_eq!(
            ready_to_land(&changes, false, true),
            1,
            "only the bottom without native stacks"
        );

        pr_mut(&mut changes[1]).pending_checks = vec!["build".into()];
        assert_eq!(ready_to_land(&changes, true, true), 1);
        pr_mut(&mut changes[0]).unresolved_threads = 1;
        assert_eq!(ready_to_land(&changes, true, true), 0);
    }

    #[test]
    fn ready_to_land_needs_approval_only_when_required() {
        let mut changes = native_stack();
        pr_mut(&mut changes[0]).review = ReviewDecision::None;
        assert_eq!(ready_to_land(&changes, true, true), 0);
        assert_eq!(ready_to_land(&changes, true, false), 3);
        pr_mut(&mut changes[0]).review = ReviewDecision::Required;
        assert_eq!(ready_to_land(&changes, true, false), 0);
    }

    #[test]
    fn ready_to_land_skips_stale_drafts_and_unsubmitted() {
        let mut changes = native_stack();
        changes[1].differs_from_pr = true;
        assert_eq!(ready_to_land(&changes, true, true), 1);
        let mut changes = native_stack();
        pr_mut(&mut changes[0]).draft = true;
        assert_eq!(ready_to_land(&changes, true, true), 0);
        let mut changes = native_stack();
        changes[0].pr = None;
        assert_eq!(ready_to_land(&changes, true, true), 0);
    }
}
