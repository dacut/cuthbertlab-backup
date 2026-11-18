//! Backs up everything GitHub stores about a repository outside of git:
//! issues, pull requests, reviews, comments, events, discussions, releases
//! (with their binaries), attachments, labels, milestones, settings, and so on.
//!
//! Each kind of data is one "collection": a gzipped JSON array of the objects
//! exactly as the API returned them, at `repos/<repo>/metadata/<name>.json.gz`.

use {
    crate::{
        Context, attachments,
        event::{Task, TaskResult},
        github::{ACCEPT_JSON, unavailable_status},
        graphql, releases, util,
    },
    anyhow::{Context as _, Result},
    futures::{StreamExt as _, TryStreamExt as _},
    serde_json::{Value, json},
    std::collections::{BTreeMap, BTreeSet},
};

/// A REST list endpoint under `/repos/{owner}/{repo}/`.
struct Collection {
    /// File name (without extension) and manifest key.
    name: &'static str,
    path: &'static str,
    accept: &'static str,
    /// Whether an ordinary read-only token may legitimately be refused this.
    /// Optional collections that come back 403/404 are recorded as skipped;
    /// required ones fail the task. Any other error fails the task either way.
    optional: bool,
    /// Whether to look through the text for attachment links.
    has_attachments: bool,
}

const fn required(name: &'static str, path: &'static str) -> Collection {
    Collection { name, path, accept: ACCEPT_JSON, optional: false, has_attachments: false }
}

const fn optional(name: &'static str, path: &'static str) -> Collection {
    Collection { name, path, accept: ACCEPT_JSON, optional: true, has_attachments: false }
}

const fn with_attachments(mut collection: Collection) -> Collection {
    collection.has_attachments = true;
    collection
}

const COLLECTIONS: &[Collection] = &[
    required("labels", "labels"),
    required("milestones", "milestones?state=all&sort=created&direction=asc"),
    // The issues list includes pull requests (GitHub models a PR as an issue
    // plus extra data), which is what carries their labels, assignees,
    // milestone and reactions.
    with_attachments(required("issues", "issues?state=all&sort=created&direction=asc")),
    with_attachments(required("issue_comments", "issues/comments?sort=created&direction=asc")),
    required("issue_events", "issues/events"),
    with_attachments(required("pulls", "pulls?state=all&sort=created&direction=asc")),
    with_attachments(required("pull_review_comments", "pulls/comments?sort=created&direction=asc")),
    with_attachments(required("commit_comments", "comments")),
    with_attachments(required("releases", "releases")),
    required("branches", "branches"),
    required("forks", "forks?sort=oldest"),
    required("deployments", "deployments"),
    // Who starred and who watches. GitHub answers 404 for these on some
    // repositories even though they are public, so they cannot be required.
    optional("subscribers", "subscribers"),
    Collection {
        name: "stargazers",
        path: "stargazers",
        // This media type adds the time each star was given.
        accept: "application/vnd.github.star+json",
        optional: true,
        has_attachments: false,
    },
    optional("environments", "environments"),
    optional("rulesets", "rulesets?includes_parents=true"),
    optional("security_advisories", "security-advisories"),
    optional("workflows", "actions/workflows"),
    // Administrative settings: readable only with admin rights on the repository.
    optional("collaborators", "collaborators?affiliation=all"),
    optional("teams", "teams"),
    optional("webhooks", "hooks"),
    optional("deploy_keys", "keys"),
    optional("autolinks", "autolinks"),
    optional("actions_variables", "actions/variables"),
    // Names only: GitHub never reveals secret values.
    optional("actions_secret_names", "actions/secrets"),
];

/// How many collections (or per-item lookups) are in flight at once. Actual
/// request concurrency is bounded separately, by the client.
const FAN_OUT: usize = 8;

enum Outcome {
    Fetched(Vec<Value>),
    Skipped(String),
}

pub async fn backup(ctx: &Context, repo: &str) -> Result<TaskResult> {
    let mut result = TaskResult::new(Task::Metadata { repo: repo.to_string() });
    let org = &ctx.config.org;
    let api = format!("repos/{org}/{repo}");
    let dir = format!("repos/{repo}/metadata");

    let info = ctx.github.get_json(&api).await.with_context(|| format!("fetching {org}/{repo}"))?;
    ctx.storage.put_json(&format!("{dir}/repo.json"), &info).await?;

    // Three independent crawls, run side by side: the plain REST lists; the
    // GraphQL view of pull requests, which supplies their reviews; and the
    // list of issue numbers. The last two also serve as a cross-check on the
    // REST lists.
    let rest = futures::stream::iter(COLLECTIONS)
        .map(|collection| {
            let api = &api;
            async move {
                let path = format!("{api}/{}", collection.path);
                let outcome = tolerate(ctx.github.paginate_as(&path, collection.accept).await, collection.optional)
                    .with_context(|| format!("fetching {}", collection.name))?;
                anyhow::Ok((collection.name, outcome))
            }
        })
        .buffer_unordered(FAN_OUT)
        .try_collect::<BTreeMap<&'static str, Outcome>>();
    let pull_details = async {
        graphql::pull_requests(&ctx.github, org, repo).await.context("fetching pull request reviews and details")
    };
    let issue_numbers = async { graphql::issue_numbers(&ctx.github, org, repo).await.context("listing issue numbers") };
    let (mut fetched, pull_details, issue_numbers) = tokio::try_join!(rest, pull_details, issue_numbers)?;

    let pull_numbers: Vec<u64> = pull_details.iter().filter_map(|pull| pull["number"].as_u64()).collect();
    let reconciled = reconcile(ctx, &api, &mut fetched, &issue_numbers, &pull_numbers)
        .await
        .context("cross-checking the issue and pull request lists")?;
    let reviews: usize = pull_details.iter().map(|pull| pull["reviews"].as_array().map_or(0, Vec::len)).sum();
    fetched.insert("pull_details", Outcome::Fetched(pull_details));

    // Lists that hang off the ones above.
    fetched.insert("branch_protection", branch_protection(ctx, &api, items(&fetched, "branches")).await?);
    if let Outcome::Fetched(rulesets) = &fetched["rulesets"] {
        let detailed = ruleset_details(ctx, &api, rulesets).await?;
        fetched.insert("rulesets", Outcome::Fetched(detailed));
    }
    fetched.insert("pages", single(ctx, &format!("{api}/pages")).await?);

    if info["has_discussions"].as_bool().unwrap_or(false) {
        let discussions = graphql::discussions(&ctx.github, org, repo).await.context("fetching discussions")?;
        let categories =
            graphql::discussion_categories(&ctx.github, org, repo).await.context("fetching discussion categories")?;
        fetched.insert("discussions", Outcome::Fetched(discussions));
        fetched.insert("discussion_categories", Outcome::Fetched(categories));
    } else {
        fetched.insert("discussions", Outcome::Skipped("discussions are not enabled for this repository".into()));
    }

    // Store.
    let mut collections = serde_json::Map::new();
    let mut skipped = serde_json::Map::new();
    let mut refused = Vec::new();
    for (name, outcome) in &fetched {
        match outcome {
            Outcome::Fetched(list) => {
                let changed = ctx.storage.put_collection(&format!("{dir}/{name}.json.gz"), list).await?;
                collections.insert(name.to_string(), json!({ "count": list.len(), "changed": changed }));
            }
            Outcome::Skipped(reason) => {
                if reason.starts_with("HTTP 403") || reason.starts_with("HTTP 404") {
                    refused.push(*name);
                }
                skipped.insert(name.to_string(), json!(reason));
            }
        }
    }
    // The issue list mixes in pull requests; count only true issues.
    let issues = items(&fetched, "issues").iter().filter(|issue| issue.get("pull_request").is_none()).count();
    result.set("issues", issues);
    result.set("pulls", items(&fetched, "pulls").len());
    result.set("pull_reviews", reviews);
    for name in ["issue_comments", "pull_review_comments", "discussions", "releases"] {
        if let Some(Outcome::Fetched(list)) = fetched.get(name) {
            result.set(name, list.len());
        }
    }
    if reconciled.recovered > 0 {
        result.set("fetched_individually", reconciled.recovered);
    }
    if !reconciled.unresolved.is_empty() {
        result.warn(format!(
            "GitHub reports {} issue or pull request number(s) that it would not return, so they are NOT backed up: {:?}",
            reconciled.unresolved.len(),
            &reconciled.unresolved[..reconciled.unresolved.len().min(20)]
        ));
    }
    if !refused.is_empty() {
        result.warn(format!(
            "not backed up, because the token is not allowed to read them (most need repository admin rights): {}",
            refused.join(", ")
        ));
    }

    // Binaries: release assets, then files attached to issues and the like.
    let assets =
        releases::backup_assets(ctx, repo, items(&fetched, "releases")).await.context("backing up release assets")?;
    result.set("release_assets", assets.total);
    result.set("release_assets_uploaded", assets.uploaded);

    let mut attachment_summary = Value::Null;
    if ctx.config.backup_attachments {
        let mut links = attachments::Links::default();
        for collection in COLLECTIONS.iter().filter(|collection| collection.has_attachments) {
            links.scan(items(&fetched, collection.name));
        }
        for name in ["pull_details", "discussions"] {
            links.scan(items(&fetched, name));
        }
        let outcome = attachments::backup(ctx, repo, links).await.context("backing up attachments")?;
        let changed = ctx.storage.put_collection(&format!("{dir}/attachments.json.gz"), &outcome.index).await?;
        collections.insert("attachments".into(), json!({ "count": outcome.index.len(), "changed": changed }));
        result.set("attachments", outcome.index.len());
        result.set("attachments_downloaded", outcome.downloaded);
        if outcome.failed > 0 {
            result.set("attachments_failed", outcome.failed);
            result.warn(format!(
                "{} of {} attachment links could not be downloaded (usually because the upload was deleted); \
                 see {dir}/attachments.json.gz. First failure: {}",
                outcome.failed,
                outcome.index.len(),
                outcome.first_failure.as_deref().unwrap_or("unknown")
            ));
        }
        attachment_summary = json!({
            "links": outcome.index.len(),
            "downloaded_this_run": outcome.downloaded,
            "failed": outcome.failed,
        });
    }

    let manifest = json!({
        "repository": format!("{org}/{repo}"),
        "backed_up_at": util::now_rfc3339(),
        "tool_version": env!("CARGO_PKG_VERSION"),
        "collections": collections,
        "skipped": skipped,
        "cross_check": {
            "issues": issue_numbers.len(),
            "pulls": pull_numbers.len(),
            "fetched_individually": reconciled.recovered,
            "unresolved": reconciled.unresolved,
        },
        "release_assets": {
            "total": assets.total,
            "uploaded_this_run": assets.uploaded,
            "bytes": assets.bytes,
        },
        "attachments": attachment_summary,
        "warnings": result.warnings,
    });
    ctx.storage.put_json(&format!("{dir}/manifest.json"), &manifest).await?;
    Ok(result)
}

/// Sorts a fetch result into "have it", "legitimately cannot have it", or failure.
fn tolerate(result: Result<Vec<Value>>, optional: bool) -> Result<Outcome> {
    match result {
        Ok(list) => Ok(Outcome::Fetched(list)),
        Err(e) => match unavailable_status(&e) {
            // GitHub answers 410 Gone for a feature that is switched off, such
            // as the issue tracker of a repository with issues disabled.
            Some(410) => Ok(Outcome::Skipped("HTTP 410: this feature is disabled for the repository".into())),
            Some(status) if optional => Ok(Outcome::Skipped(format!("HTTP {status}: not readable with this token"))),
            _ => Err(e),
        },
    }
}

fn items<'a>(fetched: &'a BTreeMap<&'static str, Outcome>, name: &str) -> &'a [Value] {
    match fetched.get(name) {
        Some(Outcome::Fetched(list)) => list,
        _ => &[],
    }
}

/// Makes the `issues` and `pulls` collections complete.
///
/// GitHub's REST lists are not reliable on their own. The issue list is
/// documented to include every pull request as well, but in practice leaves
/// many out (hundreds, on a large repository) with no pattern and no error.
/// So both lists are checked against the numbers GraphQL reports, and anything
/// GraphQL knows about that a list left out is fetched by number.
///
/// Pull requests missing from the *issue* list are deliberately not chased:
/// each is fully described by the `pulls` and `pull_details` collections, and
/// fetching the issue view of each would cost one request apiece on every run.
async fn reconcile(
    ctx: &Context,
    api: &str,
    fetched: &mut BTreeMap<&'static str, Outcome>,
    issue_numbers: &[u64],
    pull_numbers: &[u64],
) -> Result<Reconciled> {
    let number = |value: &Value| value["number"].as_u64();
    let mut reconciled = Reconciled::default();

    for (name, endpoint, expected) in [("issues", "issues", issue_numbers), ("pulls", "pulls", pull_numbers)] {
        let listed: BTreeSet<u64> = items(fetched, name).iter().filter_map(number).collect();
        let missing: Vec<u64> = expected.iter().copied().filter(|n| !listed.contains(n)).collect();
        if missing.is_empty() {
            continue;
        }

        let looked_up: Vec<(u64, Option<Value>)> = futures::stream::iter(missing)
            .map(|n| async move {
                match ctx.github.get_json(&format!("{api}/{endpoint}/{n}")).await {
                    Ok(item) => Ok((n, Some(item))),
                    Err(e) if unavailable_status(&e).is_some() => Ok((n, None)),
                    Err(e) => Err(e.context(format!("fetching #{n}, which the {name} list left out"))),
                }
            })
            .buffer_unordered(FAN_OUT)
            .try_collect()
            .await?;

        let mut all: Vec<Value> = match fetched.remove(name) {
            Some(Outcome::Fetched(list)) => list,
            _ => Vec::new(),
        };
        for (n, item) in looked_up {
            match item {
                Some(item) => {
                    reconciled.recovered += 1;
                    all.push(item);
                }
                None => reconciled.unresolved.push(n),
            }
        }
        all.sort_by_key(number);
        fetched.insert(name, Outcome::Fetched(all));
    }

    if reconciled.recovered > 0 {
        tracing::info!(count = reconciled.recovered, "fetched items individually because a list left them out");
    }
    reconciled.unresolved.sort_unstable();
    Ok(reconciled)
}

#[derive(Default)]
struct Reconciled {
    /// Issues and pull requests that had to be fetched one at a time.
    recovered: usize,
    /// Numbers GraphQL reports but REST would not return at all.
    unresolved: Vec<u64>,
}

/// Full protection settings of each protected branch (admin only).
async fn branch_protection(ctx: &Context, api: &str, branches: &[Value]) -> Result<Outcome> {
    let mut protections = Vec::new();
    for branch in branches.iter().filter(|branch| branch["protected"].as_bool().unwrap_or(false)) {
        let name = branch["name"].as_str().context("branch without a name")?;
        let path = format!("{api}/branches/{}/protection", encode_path_segment(name));
        match ctx.github.get_json(&path).await {
            Ok(protection) => protections.push(json!({ "branch": name, "protection": protection })),
            Err(e) => match unavailable_status(&e) {
                // Every branch will be refused alike; no point asking again.
                Some(status) => return Ok(Outcome::Skipped(format!("HTTP {status}: not readable with this token"))),
                None => return Err(e.context(format!("fetching protection of branch {name}"))),
            },
        }
    }
    Ok(Outcome::Fetched(protections))
}

/// The ruleset list only summarizes; fetch each ruleset to get its rules.
async fn ruleset_details(ctx: &Context, api: &str, rulesets: &[Value]) -> Result<Vec<Value>> {
    let mut detailed = Vec::with_capacity(rulesets.len());
    for ruleset in rulesets {
        let full = match ruleset["id"].as_u64() {
            // Rulesets inherited from the organization are fetched via the
            // repository too; if that is refused, keep the summary.
            Some(id) => match ctx.github.get_json(&format!("{api}/rulesets/{id}?includes_parents=true")).await {
                Ok(full) => full,
                Err(e) if unavailable_status(&e).is_some() => ruleset.clone(),
                Err(e) => return Err(e.context(format!("fetching ruleset {id}"))),
            },
            None => ruleset.clone(),
        };
        detailed.push(full);
    }
    Ok(detailed)
}

/// A single optional object, stored as a list of zero or one.
async fn single(ctx: &Context, path: &str) -> Result<Outcome> {
    match ctx.github.get_json(path).await {
        Ok(value) => Ok(Outcome::Fetched(vec![value])),
        Err(e) if unavailable_status(&e) == Some(404) => Ok(Outcome::Fetched(Vec::new())),
        Err(e) => tolerate(Err(e), true),
    }
}

/// Percent-encodes one URL path segment (branch names may contain `/`, `#`, ...).
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use {super::*, crate::github::ApiError};

    fn api_error(status: u16) -> anyhow::Error {
        ApiError { status, url: "https://api.github.com/x".into(), message: "nope".into() }.into()
    }

    #[test]
    fn collection_names_are_unique() {
        let mut names: Vec<_> = COLLECTIONS.iter().map(|collection| collection.name).collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn refusals_are_tolerated_only_for_optional_collections() {
        assert!(matches!(tolerate(Err(api_error(403)), true), Ok(Outcome::Skipped(_))));
        assert!(matches!(tolerate(Err(api_error(404)), true), Ok(Outcome::Skipped(_))));
        assert!(tolerate(Err(api_error(403)), false).is_err());
        assert!(tolerate(Err(api_error(404)), false).is_err());
    }

    #[test]
    fn disabled_features_are_skipped_even_when_required() {
        assert!(matches!(tolerate(Err(api_error(410)), false), Ok(Outcome::Skipped(_))));
    }

    #[test]
    fn real_failures_are_never_tolerated() {
        for optional in [true, false] {
            assert!(tolerate(Err(api_error(500)), optional).is_err());
            assert!(tolerate(Err(api_error(401)), optional).is_err());
            assert!(tolerate(Err(anyhow::anyhow!("connection reset")), optional).is_err());
        }
    }

    #[test]
    fn branch_names_are_encoded_for_urls() {
        assert_eq!(encode_path_segment("master"), "master");
        assert_eq!(encode_path_segment("feature/x#1"), "feature%2Fx%231");
    }
}
