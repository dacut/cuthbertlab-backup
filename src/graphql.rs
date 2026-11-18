//! The parts of GitHub that only exist in the GraphQL API: Discussions and
//! Projects (v2).
//!
//! Every connection is paginated to the end. Nested connections (comments in a
//! discussion, replies to a comment, items in a project) are fetched a first
//! page at a time with their parent and then topped up individually, so nothing
//! is silently truncated at a page boundary.

use {
    crate::github::GitHub,
    anyhow::{Context as _, Result},
    serde_json::{Value, json},
};

const DISCUSSION_FIELDS: &str = r#"
fragment Actor on Actor { login url }
fragment Reactions on Reactable { reactionGroups { content reactors { totalCount } } }
fragment CommentFields on DiscussionComment {
  id databaseId url body createdAt updatedAt lastEditedAt publishedAt
  author { ...Actor } editor { ...Actor } authorAssociation
  isAnswer isMinimized minimizedReason deletedAt upvoteCount
  ...Reactions
}
"#;

const DISCUSSIONS: &str = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    discussions(first: 25, after: $cursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id databaseId number title url body createdAt updatedAt lastEditedAt publishedAt
        author { ...Actor } editor { ...Actor } authorAssociation
        category { id name slug }
        labels(first: 100) { nodes { name } }
        closed closedAt stateReason locked activeLockReason upvoteCount
        isAnswered answerChosenAt answerChosenBy { ...Actor } answer { id databaseId }
        poll { question totalVoteCount options(first: 50) { nodes { option totalVoteCount } } }
        ...Reactions
        comments(first: 50) {
          pageInfo { hasNextPage endCursor }
          nodes {
            ...CommentFields
            replies(first: 50) {
              pageInfo { hasNextPage endCursor }
              nodes { ...CommentFields replyTo { id databaseId } }
            }
          }
        }
      }
    }
  }
}
"#;

const MORE_COMMENTS: &str = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    ... on Discussion {
      comments(first: 50, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          ...CommentFields
          replies(first: 50) {
            pageInfo { hasNextPage endCursor }
            nodes { ...CommentFields replyTo { id databaseId } }
          }
        }
      }
    }
  }
}
"#;

const MORE_REPLIES: &str = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    ... on DiscussionComment {
      replies(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { ...CommentFields replyTo { id databaseId } }
      }
    }
  }
}
"#;

const DISCUSSION_CATEGORIES: &str = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    discussionCategories(first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes { id name slug description emoji isAnswerable createdAt updatedAt }
    }
  }
}
"#;

const PULL_REQUESTS: &str = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 50, after: $cursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id number url
        additions deletions changedFiles
        mergedAt mergedBy { ...Actor }
        reviewDecision
        closingIssuesReferences(first: 100) { nodes { number repository { nameWithOwner } } }
        ...Reactions
        reviews(first: 50) {
          pageInfo { hasNextPage endCursor }
          nodes { ...ReviewFields }
        }
      }
    }
  }
}
"#;

const MORE_REVIEWS: &str = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    ... on PullRequest {
      reviews(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { ...ReviewFields }
      }
    }
  }
}
"#;

const REVIEW_FIELDS: &str = r#"
fragment Actor on Actor { login url }
fragment Reactions on Reactable { reactionGroups { content reactors { totalCount } } }
fragment ReviewFields on PullRequestReview {
  id databaseId url state body
  createdAt submittedAt updatedAt lastEditedAt
  author { ...Actor } authorAssociation
  commit { oid }
  comments { totalCount }
  ...Reactions
}
"#;

const ISSUE_NUMBERS: &str = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    issues(first: 100, after: $cursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { number }
    }
  }
}
"#;

const PROJECTS: &str = r#"
query($org: String!, $cursor: String) {
  organization(login: $org) {
    projectsV2(first: 20, after: $cursor, orderBy: {field: NUMBER, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        id databaseId number title shortDescription readme url public template
        closed closedAt createdAt updatedAt
        creator { login url }
        repositories(first: 100) { nodes { nameWithOwner } }
        fields(first: 100) {
          nodes {
            __typename
            ... on ProjectV2FieldCommon { id databaseId name dataType createdAt updatedAt }
            ... on ProjectV2SingleSelectField { options { id name color description } }
            ... on ProjectV2IterationField {
              configuration {
                duration startDay
                iterations { id title startDate duration }
                completedIterations { id title startDate duration }
              }
            }
          }
        }
        views(first: 100) {
          nodes { id databaseId number name layout filter createdAt updatedAt }
        }
      }
    }
  }
}
"#;

// Page sizes here are bounded by GitHub's limit of 500,000 "possible nodes"
// per query, which counts every nested connection at its maximum size: this
// asks for at most 25 x (50 x 110 + 20), about 138,000. A project cannot have
// more than 50 fields, so `fieldValues(first: 50)` is always complete.
const PROJECT_ITEMS: &str = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    ... on ProjectV2 {
      items(first: 25, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id databaseId type isArchived createdAt updatedAt
          creator { login url }
          content {
            __typename
            ... on Issue { number title url state repository { nameWithOwner } }
            ... on PullRequest { number title url state repository { nameWithOwner } }
            ... on DraftIssue {
              id title body createdAt updatedAt
              creator { login url }
              assignees(first: 20) { nodes { login } }
            }
          }
          fieldValues(first: 50) {
            nodes {
              __typename
              ... on ProjectV2ItemFieldTextValue { text field { ...FieldName } }
              ... on ProjectV2ItemFieldNumberValue { number field { ...FieldName } }
              ... on ProjectV2ItemFieldDateValue { date field { ...FieldName } }
              ... on ProjectV2ItemFieldSingleSelectValue { name optionId field { ...FieldName } }
              ... on ProjectV2ItemFieldIterationValue { title iterationId startDate duration field { ...FieldName } }
              ... on ProjectV2ItemFieldLabelValue { labels(first: 50) { nodes { name } } field { ...FieldName } }
              ... on ProjectV2ItemFieldUserValue { users(first: 20) { nodes { login } } field { ...FieldName } }
              ... on ProjectV2ItemFieldMilestoneValue { milestone { number title } field { ...FieldName } }
              ... on ProjectV2ItemFieldRepositoryValue { repository { nameWithOwner } field { ...FieldName } }
              ... on ProjectV2ItemFieldPullRequestValue {
                pullRequests(first: 20) { nodes { number url } }
                field { ...FieldName }
              }
              ... on ProjectV2ItemFieldReviewerValue {
                reviewers(first: 20) { nodes { __typename ... on User { login } ... on Team { slug } ... on Bot { login } } }
                field { ...FieldName }
              }
            }
          }
        }
      }
    }
  }
}
fragment FieldName on ProjectV2FieldConfiguration { ... on ProjectV2FieldCommon { name } }
"#;

/// All of a repository's discussions, each with its complete comment tree.
///
/// In the result, `comments` and `replies` are plain arrays rather than
/// GraphQL connections.
pub async fn discussions(github: &GitHub, owner: &str, name: &str) -> Result<Vec<Value>> {
    let first_page = format!("{DISCUSSIONS}{DISCUSSION_FIELDS}");
    let more_comments = format!("{MORE_COMMENTS}{DISCUSSION_FIELDS}");
    let more_replies = format!("{MORE_REPLIES}{DISCUSSION_FIELDS}");

    let mut discussions = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = github.graphql(&first_page, json!({ "owner": owner, "name": name, "cursor": cursor })).await?;
        let mut connection = take(data, &["repository", "discussions"])?;
        let page = Page::from(&mut connection)?;

        for mut discussion in page.nodes {
            let discussion_id = id_of(&discussion)?;
            let mut comments = complete(github, &more_comments, &discussion_id, "comments", &mut discussion).await?;
            for comment in &mut comments {
                let comment_id = id_of(comment)?;
                let replies = complete(github, &more_replies, &comment_id, "replies", comment).await?;
                comment["replies"] = Value::Array(replies);
            }
            discussion["comments"] = Value::Array(comments);
            flatten(&mut discussion, "labels");
            discussions.push(discussion);
        }

        match page.next {
            Some(next) => cursor = Value::String(next),
            None => break,
        }
    }
    Ok(discussions)
}

/// One entry per pull request, holding what the REST pull request list does
/// not: every review (approvals, change requests, review summaries), who
/// merged it, the size of the diff, the issues it closes, and reactions.
///
/// REST can only return reviews one pull request at a time, which on a large
/// repository costs more requests than everything else put together. This
/// fetches them fifty pull requests to a page.
///
/// In the result, `reviews` and `closingIssuesReferences` are plain arrays.
pub async fn pull_requests(github: &GitHub, owner: &str, name: &str) -> Result<Vec<Value>> {
    let first_page = format!("{PULL_REQUESTS}{REVIEW_FIELDS}");
    let more_reviews = format!("{MORE_REVIEWS}{REVIEW_FIELDS}");

    let mut pulls = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = github.graphql(&first_page, json!({ "owner": owner, "name": name, "cursor": cursor })).await?;
        let mut connection = take(data, &["repository", "pullRequests"])?;
        let page = Page::from(&mut connection)?;

        for mut pull in page.nodes {
            let pull_id = id_of(&pull)?;
            let reviews = complete(github, &more_reviews, &pull_id, "reviews", &mut pull).await?;
            pull["reviews"] = Value::Array(reviews);
            flatten(&mut pull, "closingIssuesReferences");
            pulls.push(pull);
        }

        match page.next {
            Some(next) => cursor = Value::String(next),
            None => break,
        }
    }
    Ok(pulls)
}

/// The number of every issue (not pull request) in the repository. An
/// independent source to check the REST issue list against.
pub async fn issue_numbers(github: &GitHub, owner: &str, name: &str) -> Result<Vec<u64>> {
    let mut numbers = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = github.graphql(ISSUE_NUMBERS, json!({ "owner": owner, "name": name, "cursor": cursor })).await?;
        let mut connection = take(data, &["repository", "issues"])?;
        let page = Page::from(&mut connection)?;
        for node in &page.nodes {
            numbers.push(node["number"].as_u64().context("issue without a number")?);
        }
        match page.next {
            Some(next) => cursor = Value::String(next),
            None => break,
        }
    }
    Ok(numbers)
}

/// The categories discussions are filed under.
pub async fn discussion_categories(github: &GitHub, owner: &str, name: &str) -> Result<Vec<Value>> {
    let mut categories = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data =
            github.graphql(DISCUSSION_CATEGORIES, json!({ "owner": owner, "name": name, "cursor": cursor })).await?;
        let mut connection = take(data, &["repository", "discussionCategories"])?;
        let page = Page::from(&mut connection)?;
        categories.extend(page.nodes);
        match page.next {
            Some(next) => cursor = Value::String(next),
            None => break,
        }
    }
    Ok(categories)
}

/// Every Project (v2) owned by the organization, open or closed, each with all
/// of its fields, views and items (draft issues included).
pub async fn org_projects(github: &GitHub, org: &str) -> Result<Vec<Value>> {
    let mut projects = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = github.graphql(PROJECTS, json!({ "org": org, "cursor": cursor })).await?;
        let mut connection = take(data, &["organization", "projectsV2"])?;
        let page = Page::from(&mut connection)?;

        for mut project in page.nodes {
            let project_id = id_of(&project)?;
            // Items are fetched separately from the start: a project can hold
            // thousands, each with many field values.
            project["items"] = json!({ "pageInfo": { "hasNextPage": true, "endCursor": null }, "nodes": [] });
            let mut items = complete(github, PROJECT_ITEMS, &project_id, "items", &mut project).await?;
            for item in &mut items {
                flatten(item, "fieldValues");
            }
            project["items"] = Value::Array(items);
            for field in ["fields", "views", "repositories"] {
                flatten(&mut project, field);
            }
            projects.push(project);
        }

        match page.next {
            Some(next) => cursor = Value::String(next),
            None => break,
        }
    }
    Ok(projects)
}

/// One page of a GraphQL connection.
struct Page {
    nodes: Vec<Value>,
    /// Cursor for the next page, if there is one.
    next: Option<String>,
}

impl Page {
    fn from(connection: &mut Value) -> Result<Self> {
        let nodes = match connection.get_mut("nodes").map(Value::take) {
            // Nodes can be null when an individual item is not visible to the token.
            Some(Value::Array(nodes)) => nodes.into_iter().filter(|node| !node.is_null()).collect(),
            _ => anyhow::bail!("GraphQL connection has no nodes: {connection}"),
        };
        let info = &connection["pageInfo"];
        let next = if info["hasNextPage"].as_bool().unwrap_or(false) {
            Some(info["endCursor"].as_str().context("GraphQL connection has more pages but no cursor")?.to_string())
        } else {
            None
        };
        Ok(Self { nodes, next })
    }
}

/// Takes the first page of `parent[field]` (already fetched along with the
/// parent) and keeps requesting further pages via `query` until the connection
/// is exhausted. Returns all nodes.
async fn complete(
    github: &GitHub,
    query: &str,
    parent_id: &str,
    field: &str,
    parent: &mut Value,
) -> Result<Vec<Value>> {
    let mut connection = parent.get_mut(field).map(Value::take).unwrap_or(Value::Null);
    let mut page = Page::from(&mut connection).with_context(|| format!("reading {field} of {parent_id}"))?;
    let mut nodes = std::mem::take(&mut page.nodes);
    while let Some(cursor) = page.next {
        let data = github.graphql(query, json!({ "id": parent_id, "cursor": cursor })).await?;
        let mut connection = take(data, &["node", field])?;
        page = Page::from(&mut connection)?;
        nodes.append(&mut page.nodes);
    }
    Ok(nodes)
}

/// Replaces the connection at `value[field]` (`{"nodes": [...]}`) with its
/// bare list of nodes. Only for connections fetched in a single page.
fn flatten(value: &mut Value, field: &str) {
    if let Some(connection) = value.get_mut(field)
        && let Some(nodes) = connection.get_mut("nodes").map(Value::take)
    {
        *connection = nodes;
    }
}

fn take(mut data: Value, path: &[&str]) -> Result<Value> {
    let mut current = &mut data;
    for key in path {
        current = current
            .get_mut(*key)
            .filter(|value| !value.is_null())
            .with_context(|| format!("GraphQL response has no {}", path.join(".")))?;
    }
    Ok(current.take())
}

fn id_of(node: &Value) -> Result<String> {
    Ok(node["id"].as_str().context("GraphQL node has no id")?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_report_whether_more_follow() {
        let mut last = json!({"pageInfo": {"hasNextPage": false, "endCursor": "abc"}, "nodes": [{"id": "1"}, null]});
        let page = Page::from(&mut last).unwrap();
        assert_eq!(page.nodes, vec![json!({"id": "1"})]);
        assert_eq!(page.next, None);

        let mut more = json!({"pageInfo": {"hasNextPage": true, "endCursor": "abc"}, "nodes": []});
        assert_eq!(Page::from(&mut more).unwrap().next.as_deref(), Some("abc"));

        // Claiming more pages without saying where they start is an error,
        // not a reason to stop early.
        let mut broken = json!({"pageInfo": {"hasNextPage": true, "endCursor": null}, "nodes": []});
        assert!(Page::from(&mut broken).is_err());
        assert!(Page::from(&mut json!({"pageInfo": {}})).is_err());
    }

    #[test]
    fn flatten_unwraps_connections() {
        let mut value = json!({"labels": {"nodes": [{"name": "bug"}]}, "title": "t"});
        flatten(&mut value, "labels");
        flatten(&mut value, "missing");
        assert_eq!(value, json!({"labels": [{"name": "bug"}], "title": "t"}));
    }

    #[test]
    fn take_walks_a_path_and_rejects_nulls() {
        let data = json!({"repository": {"discussions": {"nodes": []}}});
        assert_eq!(take(data, &["repository", "discussions"]).unwrap(), json!({"nodes": []}));
        assert!(take(json!({"repository": null}), &["repository", "discussions"]).is_err());
    }
}
