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

use crate::github::GitHubStack;

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
}
