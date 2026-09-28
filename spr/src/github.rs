/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use graphql_client::{GraphQLQuery, Response};
use serde::Deserialize;

use crate::{
    error::{Error, Result, ResultExt},
    message::{MessageSection, MessageSectionsMap, build_github_body, parse_message},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Clone)]
pub struct GitHub {
    config: crate::config::Config,
    repo_path: PathBuf,
    graphql_client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    pub title: String,
    pub body: Option<String>,
    pub sections: MessageSectionsMap,
    pub base: GitHubBranch,
    pub head: GitHubBranch,
    pub base_oid: git2::Oid,
    pub head_oid: git2::Oid,
    /// The head commit as GitHub reports it. Unlike `head_oid`, which comes
    /// from the local remote-tracking ref, this is still known after the
    /// Pull Request's branch has been deleted.
    pub github_head_oid: Option<git2::Oid>,
    pub is_draft: bool,
    pub merge_commit: Option<git2::Oid>,
    pub reviewers: HashMap<String, ReviewStatus>,
    pub review_status: Option<ReviewStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStatus {
    Requested,
    Approved,
    Rejected,
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<PullRequestState>,
}

impl PullRequestUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.base.is_none() && self.state.is_none()
    }

    pub fn update_message(&mut self, pull_request: &PullRequest, message: &MessageSectionsMap) {
        let title = message.get(&MessageSection::Title);
        if title.is_some() && title != Some(&pull_request.title) {
            self.title = title.cloned();
        }

        let body = build_github_body(message);
        if pull_request.body.as_ref() != Some(&body) {
            self.body = Some(body);
        }
    }
}

#[derive(serde::Serialize, Default, Debug)]
pub struct PullRequestRequestReviewers {
    pub reviewers: Vec<String>,
    pub team_reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Closed,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct UserWithName {
    pub login: String,
    pub name: Option<String>,
    #[serde(default)]
    pub is_collaborator: bool,
}

#[derive(Debug, Clone)]
pub struct PullRequestMergeability {
    pub base: GitHubBranch,
    pub head_oid: git2::Oid,
    pub mergeable: Option<bool>,
    pub merge_commit: Option<git2::Oid>,
}

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestQuery;
type GitObjectID = String;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/pullrequest_mergeability_query.graphql",
    response_derives = "Debug"
)]
pub struct PullRequestMergeabilityQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/gql/schema.docs.graphql",
    query_path = "src/gql/open_pull_request_branches.graphql",
    response_derives = "Debug"
)]
pub struct OpenPullRequestBranchesQuery;

impl GitHub {
    pub fn new(
        config: crate::config::Config,
        repo_path: PathBuf,
        graphql_client: reqwest::Client,
    ) -> Self {
        Self {
            config,
            repo_path,
            graphql_client,
        }
    }

    pub async fn get_github_user(login: String) -> Result<UserWithName> {
        octocrab::instance()
            .get::<UserWithName, _, _>(format!("/users/{}", login), None::<&()>)
            .await
            .map_err(Error::from)
    }

    pub async fn get_github_team(
        owner: String,
        team: String,
    ) -> Result<octocrab::models::teams::Team> {
        octocrab::instance()
            .teams(owner)
            .get(team)
            .await
            .map_err(Error::from)
    }

    pub async fn get_pull_request(self, number: u64) -> Result<PullRequest> {
        let GitHub {
            config,
            repo_path,
            graphql_client,
        } = self;
        let repo_path = repo_path.to_str().unwrap();

        let variables = pull_request_query::Variables {
            name: config.repo.clone(),
            owner: config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestQuery::build_query(variables);
        let res = graphql_client
            .post(config.graphql_url())
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_query::ResponseData> = res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!("fetching PR #{number} failed")));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        let base = config.new_github_branch_from_ref(&pr.base_ref_name)?;
        let head = config.new_github_branch_from_ref(&pr.head_ref_name)?;

        // Fetch refs from remote using git (since we're in a colocated repo)
        let _fetch_result = tokio::process::Command::new("git")
            .args([
                "--git-dir",
                repo_path,
                "fetch",
                "--no-write-fetch-head",
                &config.remote_name,
                &format!("{}:{}", head.on_github(), head.local()),
                &format!("{}:{}", base.on_github(), base.local()),
            ])
            .output()
            .await;

        // Convert branch refs to OIDs
        let base_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["--git-dir", repo_path, "rev-parse", base.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let head_oid = if let Ok(output) = tokio::process::Command::new("git")
            .args(["--git-dir", repo_path, "rev-parse", head.local()])
            .output()
            .await
        {
            if output.status.success() {
                let oid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                git2::Oid::from_str(&oid_str).unwrap_or(git2::Oid::zero())
            } else {
                git2::Oid::zero()
            }
        } else {
            git2::Oid::zero()
        };

        let mut sections = parse_message(&pr.body, MessageSection::Summary);

        let title = pr.title.trim().to_string();
        sections.insert(
            MessageSection::Title,
            if title.is_empty() {
                String::from("(untitled)")
            } else {
                title
            },
        );

        sections.insert(MessageSection::PullRequest, config.pull_request_url(number));

        let reviewers: HashMap<String, ReviewStatus> = pr
            .latest_opinionated_reviews
            .iter()
            .flat_map(|all_reviews| &all_reviews.nodes)
            .flatten()
            .flatten()
            .flat_map(|review| {
                let user_name = review.author.as_ref()?.login.clone();
                let status = match review.state {
                    pull_request_query::PullRequestReviewState::APPROVED => ReviewStatus::Approved,
                    pull_request_query::PullRequestReviewState::CHANGES_REQUESTED => {
                        ReviewStatus::Rejected
                    }
                    _ => ReviewStatus::Requested,
                };
                Some((user_name, status))
            })
            .collect();

        let review_status = match pr.review_decision {
            Some(pull_request_query::PullRequestReviewDecision::APPROVED) => {
                Some(ReviewStatus::Approved)
            }
            Some(pull_request_query::PullRequestReviewDecision::CHANGES_REQUESTED) => {
                Some(ReviewStatus::Rejected)
            }
            Some(pull_request_query::PullRequestReviewDecision::REVIEW_REQUIRED) => {
                Some(ReviewStatus::Requested)
            }
            _ => None,
        };

        let requested_reviewers: Vec<String> = pr.review_requests
            .iter()
            .flat_map(|x| &x.nodes)
            .flatten()
            .flatten()
            .flat_map(|x| &x.requested_reviewer)
            .flat_map(|reviewer| {
              type UserType = pull_request_query::PullRequestQueryRepositoryPullRequestReviewRequestsNodesRequestedReviewer;
              match reviewer {
                UserType::User(user) => Some(user.login.clone()),
                UserType::Team(team) => Some(format!("#{}", team.slug)),
                _ => None,
              }
            })
            .chain(reviewers.keys().cloned())
            .collect::<HashSet<String>>() // de-duplicate
            .into_iter()
            .collect();

        sections.insert(
            MessageSection::Reviewers,
            requested_reviewers.iter().fold(String::new(), |out, slug| {
                if out.is_empty() {
                    slug.to_string()
                } else {
                    format!("{}, {}", out, slug)
                }
            }),
        );

        if review_status == Some(ReviewStatus::Approved) {
            sections.insert(
                MessageSection::ReviewedBy,
                reviewers
                    .iter()
                    .filter_map(|(k, v)| {
                        if v == &ReviewStatus::Approved {
                            Some(k)
                        } else {
                            None
                        }
                    })
                    .fold(String::new(), |out, slug| {
                        if out.is_empty() {
                            slug.to_string()
                        } else {
                            format!("{}, {}", out, slug)
                        }
                    }),
            );
        }

        Ok::<_, Error>(PullRequest {
            number: pr.number as u64,
            state: match pr.state {
                pull_request_query::PullRequestState::OPEN => PullRequestState::Open,
                _ => PullRequestState::Closed,
            },
            title: pr.title,
            body: Some(pr.body),
            sections,
            base,
            head,
            base_oid,
            head_oid,
            github_head_oid: git2::Oid::from_str(&pr.head_ref_oid).ok(),
            is_draft: pr.is_draft,
            reviewers,
            review_status,
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }

    pub async fn create_pull_request(
        &self,
        message: &MessageSectionsMap,
        base_ref_name: String,
        head_ref_name: String,
        draft: bool,
    ) -> Result<u64> {
        let number = octocrab::instance()
            .pulls(self.config.owner.clone(), self.config.repo.clone())
            .create(
                message
                    .get(&MessageSection::Title)
                    .unwrap_or(&String::new()),
                head_ref_name,
                base_ref_name,
            )
            .body(build_github_body(message))
            .draft(Some(draft))
            .send()
            .await?
            .number;

        Ok(number)
    }

    pub async fn update_pull_request(&self, number: u64, updates: PullRequestUpdate) -> Result<()> {
        octocrab::instance()
            .patch::<octocrab::models::pulls::PullRequest, _, _>(
                format!(
                    "/repos/{}/{}/pulls/{}",
                    self.config.owner, self.config.repo, number
                ),
                Some(&updates),
            )
            .await?;

        Ok(())
    }

    pub async fn request_reviewers(
        &self,
        number: u64,
        reviewers: PullRequestRequestReviewers,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Ignore {}
        let _: Ignore = octocrab::instance()
            .post(
                format!(
                    "/repos/{}/{}/pulls/{}/requested_reviewers",
                    self.config.owner, self.config.repo, number
                ),
                Some(&reviewers),
            )
            .await?;

        Ok(())
    }

    pub async fn get_pull_request_mergeability(
        &self,
        number: u64,
    ) -> Result<PullRequestMergeability> {
        let variables = pull_request_mergeability_query::Variables {
            name: self.config.repo.clone(),
            owner: self.config.owner.clone(),
            number: number as i64,
        };
        let request_body = PullRequestMergeabilityQuery::build_query(variables);
        let res = self
            .graphql_client
            .post(self.config.graphql_url())
            .json(&request_body)
            .send()
            .await?;
        let response_body: Response<pull_request_mergeability_query::ResponseData> =
            res.json().await?;

        if let Some(errors) = response_body.errors {
            let error = Err(Error::new(format!(
                "querying PR #{number} mergeability failed"
            )));
            return errors
                .into_iter()
                .fold(error, |err, e| err.context(e.to_string()));
        }

        let pr = response_body
            .data
            .ok_or_else(|| Error::new("failed to fetch PR"))?
            .repository
            .ok_or_else(|| Error::new("failed to find repository"))?
            .pull_request
            .ok_or_else(|| Error::new("failed to find PR"))?;

        Ok::<_, Error>(PullRequestMergeability {
            base: self.config.new_github_branch_from_ref(&pr.base_ref_name)?,
            head_oid: git2::Oid::from_str(&pr.head_ref_oid)?,
            mergeable: match pr.mergeable {
                pull_request_mergeability_query::MergeableState::CONFLICTING => Some(false),
                pull_request_mergeability_query::MergeableState::MERGEABLE => Some(true),
                pull_request_mergeability_query::MergeableState::UNKNOWN => None,
                _ => None,
            },
            merge_commit: pr
                .merge_commit
                .and_then(|sha| git2::Oid::from_str(&sha.oid).ok()),
        })
    }

    /// All open Pull Requests in the repository, with their head and base
    /// branch names.
    pub async fn get_open_pull_requests(&self) -> Result<Vec<OpenPullRequest>> {
        let mut pull_requests = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let variables = open_pull_request_branches_query::Variables {
                owner: self.config.owner.clone(),
                name: self.config.repo.clone(),
                first: 100,
                after: after.clone(),
            };
            let request_body = OpenPullRequestBranchesQuery::build_query(variables);
            let res = self
                .graphql_client
                .post(self.config.graphql_url())
                .json(&request_body)
                .send()
                .await?;
            let response_body: Response<open_pull_request_branches_query::ResponseData> =
                res.json().await?;

            if let Some(errors) = response_body.errors {
                let error = Err(Error::new("fetching open PR branches failed".to_string()));
                return errors
                    .into_iter()
                    .fold(error, |err, e| err.context(e.to_string()));
            }

            let prs = response_body
                .data
                .ok_or_else(|| Error::new("failed to fetch open PRs"))?
                .repository
                .ok_or_else(|| Error::new("failed to find repository"))?
                .pull_requests;

            if let Some(nodes) = prs.nodes {
                for node in nodes.into_iter().flatten() {
                    pull_requests.push(OpenPullRequest {
                        number: node.number as u64,
                        head_ref_name: node.head_ref_name,
                        base_ref_name: node.base_ref_name,
                    });
                }
            }

            if prs.page_info.has_next_page {
                after = prs.page_info.end_cursor;
            } else {
                break;
            }
        }

        Ok(pull_requests)
    }

    pub async fn get_open_pr_branch_names(&self) -> Result<HashSet<String>> {
        Ok(self
            .get_open_pull_requests()
            .await?
            .into_iter()
            .flat_map(|pr| [pr.head_ref_name, pr.base_ref_name])
            .collect())
    }

    /// Send a request to the GitHub REST API, using the API version that
    /// introduced stacked pull requests.
    async fn rest_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(reqwest::StatusCode, Option<serde_json::Value>)> {
        let url = format!("{}{}", self.config.github_api_url, path);
        let mut request = self
            .graphql_client
            .request(method.clone(), &url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_STACKS_API_VERSION);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        let json = if text.trim().is_empty() {
            None
        } else {
            serde_json::from_str(&text).ok()
        };

        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok((status, json));
        }

        let github_message = json
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or(text);
        Err(Error::new(format!(
            "GitHub returned {} for {} {}: {}",
            status.as_u16(),
            method,
            path,
            github_message
        )))
    }

    fn stacks_path(&self) -> String {
        format!("/repos/{}/{}/stacks", self.config.owner, self.config.repo)
    }

    /// List the repository's native GitHub stacks. Returns `None` when stacked
    /// pull requests are not enabled for the repository (GitHub answers 404).
    pub async fn list_stacks(&self) -> Result<Option<Vec<GitHubStack>>> {
        let mut stacks = Vec::new();
        let mut page = 1;
        loop {
            let (status, json) = self
                .rest_request(
                    reqwest::Method::GET,
                    &format!("{}?per_page=100&page={}", self.stacks_path(), page),
                    None,
                )
                .await?;
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let batch: Vec<GitHubStack> = serde_json::from_value(json.unwrap_or_default())
                .map_err(|e| Error::new(format!("GitHub returned an invalid stack list: {e}")))?;
            let full_page = batch.len() == 100;
            stacks.extend(batch);
            if !full_page {
                break;
            }
            page += 1;
        }
        Ok(Some(stacks))
    }

    /// Create a native GitHub stack from Pull Request numbers, bottom first.
    pub async fn create_stack(&self, pull_requests: &[u64]) -> Result<GitHubStack> {
        let (status, json) = self
            .rest_request(
                reqwest::Method::POST,
                &self.stacks_path(),
                Some(serde_json::json!({ "pull_requests": pull_requests })),
            )
            .await?;
        parse_stack_response(status, json, "creating a stack")
    }

    /// Append Pull Requests (bottom first) to the top of an existing stack.
    pub async fn add_to_stack(&self, stack: u64, pull_requests: &[u64]) -> Result<GitHubStack> {
        let (status, json) = self
            .rest_request(
                reqwest::Method::POST,
                &format!("{}/{}/add", self.stacks_path(), stack),
                Some(serde_json::json!({ "pull_requests": pull_requests })),
            )
            .await?;
        parse_stack_response(status, json, "adding to a stack")
    }

    /// Remove the open Pull Requests from a stack. The Pull Requests
    /// themselves, and their branches, stay as they are.
    pub async fn unstack(&self, stack: u64) -> Result<()> {
        let (status, _) = self
            .rest_request(
                reqwest::Method::POST,
                &format!("{}/{}/unstack", self.stacks_path(), stack),
                None,
            )
            .await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::new(format!("GitHub stack #{stack} was not found")));
        }
        Ok(())
    }

    /// Merge a Pull Request that is part of a native stack. GitHub only
    /// accepts merges of stacked Pull Requests through its asynchronous merge
    /// endpoint; this submits the merge and waits for it to finish.
    pub async fn merge_stacked_pull_request(
        &self,
        number: u64,
        expected_head: git2::Oid,
        merge_method: &str,
    ) -> Result<Option<String>> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge-async",
            self.config.owner, self.config.repo, number
        );
        let (status, json) = self
            .rest_request(
                reqwest::Method::PUT,
                &path,
                Some(serde_json::json!({
                    "merge_action": "direct_merge",
                    "merge_method": merge_method,
                    "sha": expected_head.to_string(),
                })),
            )
            .await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::new(format!(
                "GitHub could not find Pull Request #{number} to merge"
            )));
        }
        let mut merge: StackMerge = serde_json::from_value(json.unwrap_or_default())
            .map_err(|e| Error::new(format!("GitHub returned an invalid merge response: {e}")))?;

        for _ in 0..120 {
            match merge.status.as_str() {
                "merged" => return Ok(merge.details.sha),
                "failed" => {
                    return Err(Error::new(format!(
                        "GitHub could not merge Pull Request #{number}: {}",
                        merge.details.message.unwrap_or_default()
                    )));
                }
                "enqueued" => {
                    return Err(Error::new(format!(
                        "GitHub added Pull Request #{number} to the merge queue instead \
                         of merging it: {}",
                        merge.details.message.unwrap_or_default()
                    )));
                }
                _ => {}
            }
            let uuid = merge.details.uuid.clone().ok_or_else(|| {
                Error::new("GitHub's merge response did not identify the merge operation")
            })?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let (_, json) = self
                .rest_request(reqwest::Method::GET, &format!("{path}/{uuid}"), None)
                .await?;
            merge = serde_json::from_value(json.unwrap_or_default()).map_err(|e| {
                Error::new(format!("GitHub returned an invalid merge response: {e}"))
            })?;
        }
        Err(Error::new(format!(
            "Timed out waiting for GitHub to merge Pull Request #{number}"
        )))
    }
}

/// `X-GitHub-Api-Version` for the Stacks API and the asynchronous merge
/// endpoint.
pub const GITHUB_STACKS_API_VERSION: &str = "2026-03-10";

fn parse_stack_response(
    status: reqwest::StatusCode,
    json: Option<serde_json::Value>,
    what: &str,
) -> Result<GitHubStack> {
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::new(format!(
            "GitHub answered 404 while {what}; stacked pull requests may not be \
             enabled for this repository"
        )));
    }
    serde_json::from_value(json.unwrap_or_default()).map_err(|e| {
        Error::new(format!(
            "GitHub returned an invalid stack while {what}: {e}"
        ))
    })
}

#[derive(Debug, Clone)]
pub struct OpenPullRequest {
    pub number: u64,
    pub head_ref_name: String,
    pub base_ref_name: String,
}

/// A native GitHub stack, as returned by the Stacks API.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubStack {
    pub number: u64,
    pub pull_requests: Vec<GitHubStackMember>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubStackMember {
    pub number: u64,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub merged_at: Option<String>,
}

impl GitHubStackMember {
    /// Merged and closed members stay listed in a stack as history.
    pub fn is_active(&self) -> bool {
        self.merged_at.is_none() && self.state.as_deref().unwrap_or("open") == "open"
    }
}

impl GitHubStack {
    /// The open members of the stack, bottom first.
    pub fn active_pull_requests(&self) -> Vec<u64> {
        self.pull_requests
            .iter()
            .filter(|pr| pr.is_active())
            .map(|pr| pr.number)
            .collect()
    }

    pub fn contains(&self, number: u64) -> bool {
        self.pull_requests.iter().any(|pr| pr.number == number)
    }
}

#[derive(Debug, Deserialize)]
struct StackMerge {
    status: String,
    #[serde(default)]
    details: StackMergeDetails,
}

#[derive(Debug, Default, Deserialize)]
struct StackMergeDetails {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GitHubBranch {
    ref_on_github: String,
    ref_local: String,
    is_master_branch: bool,
}

impl GitHubBranch {
    pub fn new_from_ref(ghref: &str, remote_name: &str, master_branch_name: &str) -> Result<Self> {
        let ref_on_github = if ghref.starts_with("refs/heads/") {
            ghref.to_string()
        } else if ghref.starts_with("refs/") {
            return Err(Error::new(format!(
                "Ref '{ghref}' does not refer to a branch"
            )));
        } else {
            format!("refs/heads/{ghref}")
        };

        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        let branch_name = &ref_on_github[11..];
        let ref_local = format!("refs/remotes/{remote_name}/{branch_name}");
        let is_master_branch = branch_name == master_branch_name;

        Ok(Self {
            ref_on_github,
            ref_local,
            is_master_branch,
        })
    }

    pub fn new_from_branch_name(
        branch_name: &str,
        remote_name: &str,
        master_branch_name: &str,
    ) -> Self {
        Self {
            ref_on_github: format!("refs/heads/{branch_name}"),
            ref_local: format!("refs/remotes/{remote_name}/{branch_name}"),
            is_master_branch: branch_name == master_branch_name,
        }
    }

    pub fn on_github(&self) -> &str {
        &self.ref_on_github
    }

    pub fn local(&self) -> &str {
        &self.ref_local
    }

    pub fn is_master_branch(&self) -> bool {
        self.is_master_branch
    }

    pub fn branch_name(&self) -> &str {
        // The branch name is `ref_on_github` with the `refs/heads/` prefix
        // (length 11) removed
        &self.ref_on_github[11..]
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn test_new_from_ref_with_branch_name() {
        let r = GitHubBranch::new_from_ref("foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_branch_name() {
        let r =
            GitHubBranch::new_from_ref("masterbranch", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/foo", "github-remote", "masterbranch").unwrap();
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_master_ref_name() {
        let r =
            GitHubBranch::new_from_ref("refs/heads/masterbranch", "github-remote", "masterbranch")
                .unwrap();
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_branch_name() {
        let r = GitHubBranch::new_from_branch_name("foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/foo");
        assert_eq!(r.branch_name(), "foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_master_branch_name() {
        let r = GitHubBranch::new_from_branch_name("masterbranch", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/masterbranch");
        assert_eq!(r.local(), "refs/remotes/github-remote/masterbranch");
        assert_eq!(r.branch_name(), "masterbranch");
        assert!(r.is_master_branch());
    }

    #[test]
    fn test_new_from_ref_with_edge_case_ref_name() {
        let r = GitHubBranch::new_from_ref(
            "refs/heads/refs/heads/foo",
            "github-remote",
            "masterbranch",
        )
        .unwrap();
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }

    #[test]
    fn test_new_from_edge_case_branch_name() {
        let r =
            GitHubBranch::new_from_branch_name("refs/heads/foo", "github-remote", "masterbranch");
        assert_eq!(r.on_github(), "refs/heads/refs/heads/foo");
        assert_eq!(r.local(), "refs/remotes/github-remote/refs/heads/foo");
        assert_eq!(r.branch_name(), "refs/heads/foo");
        assert!(!r.is_master_branch());
    }
}
