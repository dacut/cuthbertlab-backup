//! A small GitHub API client: REST with pagination, GraphQL, and file
//! downloads, with the retry and rate-limit handling a long unattended crawl
//! needs.
//!
//! Responses are kept as raw [`serde_json::Value`]s rather than typed structs.
//! A backup should store what GitHub returned, including fields this program
//! has never heard of.

use {
    crate::{config::Config, util::truncate},
    anyhow::{Context as _, Result, bail},
    futures::{StreamExt as _, TryStreamExt as _},
    regex::Regex,
    reqwest::{
        RequestBuilder, Response, StatusCode,
        header::{ACCEPT, HeaderMap, HeaderValue, RETRY_AFTER},
    },
    serde_json::{Value, json},
    std::{
        fmt,
        path::Path,
        sync::{
            LazyLock,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    },
    tokio::{
        io::AsyncWriteExt as _,
        sync::{Mutex, Semaphore},
        time::Instant,
    },
};

pub const ACCEPT_JSON: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

/// Attempts per request for failures that are worth retrying as-is
/// (connection errors, timeouts, HTTP 5xx, truncated bodies).
const MAX_ATTEMPTS: u32 = 5;
/// How many times a single request will sit out a rate limit.
const MAX_RATE_LIMIT_WAITS: u32 = 3;
/// Minimum spacing between request starts. GitHub's secondary rate limit
/// allows 900 "points" per minute for REST reads; this stays under it.
const MIN_INTERVAL: Duration = Duration::from_millis(80);
/// Pages of one list requested at a time. Actual request concurrency is
/// bounded across the whole client, by `GITHUB_CONCURRENCY`.
const PAGE_FAN_OUT: usize = 8;

/// GitHub answered, but not with success.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub url: String,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned HTTP {}: {}", self.url, self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// A GraphQL response that carried an `errors` array.
#[derive(Debug)]
pub struct GraphQlError {
    pub kinds: Vec<String>,
    pub messages: Vec<String>,
}

impl GraphQlError {
    /// Whether the token is simply not allowed to see what was asked for.
    pub fn is_permission_problem(&self) -> bool {
        self.kinds.iter().any(|kind| kind == "INSUFFICIENT_SCOPES" || kind == "FORBIDDEN")
    }
}

impl fmt::Display for GraphQlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // GitHub repeats near-identical messages, one per offending field.
        let first = self.messages.first().map(String::as_str).unwrap_or("no message given");
        write!(f, "GraphQL query failed: {}", truncate(first, 500))?;
        if self.messages.len() > 1 {
            write!(f, " (and {} more errors)", self.messages.len() - 1)?;
        }
        Ok(())
    }
}

impl std::error::Error for GraphQlError {}

/// If `error` means "this token may not read that" or "that does not exist
/// here", returns the HTTP status. Rate limiting never looks like this: it is
/// handled (or reported as a different error) inside the client.
pub fn unavailable_status(error: &anyhow::Error) -> Option<u16> {
    error.downcast_ref::<ApiError>().map(|e| e.status).filter(|status| matches!(status, 403 | 404 | 410 | 451))
}

pub struct Downloaded {
    pub size: u64,
    pub content_type: Option<String>,
}

pub struct GitHub {
    http: reqwest::Client,
    token: String,
    api_url: String,
    graphql_url: String,
    web_url: String,
    permits: Semaphore,
    next_slot: Mutex<Instant>,
    max_rate_limit_wait: Duration,
    requests: AtomicU64,
}

impl GitHub {
    pub fn new(config: &Config, token: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("X-GitHub-Api-Version", HeaderValue::from_static(API_VERSION));
        let http = reqwest::Client::builder()
            .user_agent(concat!("github-backup-lambda/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        // GitHub Enterprise Server serves REST at /api/v3 and GraphQL at /api/graphql.
        let graphql_url = match config.api_url.strip_suffix("/v3") {
            Some(base) => format!("{base}/graphql"),
            None => format!("{}/graphql", config.api_url),
        };
        Ok(Self {
            http,
            token,
            api_url: config.api_url.clone(),
            graphql_url,
            web_url: config.web_url.clone(),
            permits: Semaphore::new(config.concurrency),
            next_slot: Mutex::new(Instant::now()),
            max_rate_limit_wait: config.max_rate_limit_wait,
            requests: AtomicU64::new(0),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn web_url(&self) -> &str {
        &self.web_url
    }

    /// Total requests sent so far, including retries.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn api(&self, path: &str) -> String {
        format!("{}/{}", self.api_url, path.trim_start_matches('/'))
    }

    /// Fetches a single REST resource.
    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let url = self.api(path);
        let (_, body) = self.get_api(&url, ACCEPT_JSON).await?;
        serde_json::from_slice(&body).with_context(|| format!("{url} did not return valid JSON"))
    }

    /// Fetches every page of a REST list.
    pub async fn paginate(&self, path: &str) -> Result<Vec<Value>> {
        self.paginate_as(path, ACCEPT_JSON).await
    }

    /// Fetches every page of a REST list, asking for a specific media type.
    pub async fn paginate_as(&self, path: &str, accept: &str) -> Result<Vec<Value>> {
        let first = self.api(path);
        let separator = if first.contains('?') { '&' } else { '?' };
        let (mut items, mut links) = self.get_page(&format!("{first}{separator}per_page=100"), accept).await?;

        // Most lists are numbered pages and say up front how many there are,
        // so the rest can be fetched side by side. One after another, the
        // event history of a busy repository alone takes minutes.
        if let Some(urls) = links.remaining_pages() {
            let pages: Vec<(Vec<Value>, PageLinks)> = futures::stream::iter(urls)
                .map(|url| async move { self.get_page(&url, accept).await })
                .buffered(PAGE_FAN_OUT)
                .try_collect()
                .await?;
            for (page, page_links) in pages {
                items.extend(page);
                links = page_links;
            }
        }

        // Lists paged by cursor can only be followed link by link. So can any
        // pages that came into being while the ones above were being fetched.
        while let Some(url) = links.next.take() {
            let (page, page_links) = self.get_page(&url, accept).await?;
            items.extend(page);
            links = page_links;
        }
        Ok(dedup_by_id(items))
    }

    /// Fetches one page of a REST list, along with where the list continues.
    async fn get_page(&self, url: &str, accept: &str) -> Result<(Vec<Value>, PageLinks)> {
        let (headers, body) = self.get_api(url, accept).await?;
        let page: Value = serde_json::from_slice(&body).with_context(|| format!("{url} did not return valid JSON"))?;
        let items = match page {
            Value::Array(items) => items,
            // Some lists arrive in an envelope: {"total_count": 2, "environments": [...]}.
            Value::Object(envelope) => envelope
                .into_iter()
                .find_map(|(_, value)| match value {
                    Value::Array(items) => Some(items),
                    _ => None,
                })
                .with_context(|| format!("{url} returned an object with no list in it"))?,
            other => bail!("{url} returned {} instead of a list", truncate(&other.to_string(), 100)),
        };

        let links = PageLinks::parse(&headers);
        for link in links.next.iter().chain(&links.last) {
            // The token goes wherever these URLs point, so do not follow them off-site.
            if !link.starts_with(&format!("{}/", self.api_url)) {
                bail!("refusing to follow pagination link to {link}");
            }
        }
        Ok((items, links))
    }

    /// Runs a GraphQL query and returns its `data`. Any entry in `errors`
    /// fails the call, even if partial data came back: a backup must not
    /// quietly store half an answer.
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let payload = json!({ "query": query, "variables": variables });
        let body = self
            .fetch_body(&self.graphql_url, || {
                self.http
                    .post(&self.graphql_url)
                    .bearer_auth(&self.token)
                    .header(ACCEPT, ACCEPT_JSON)
                    .timeout(Duration::from_secs(120))
                    .json(&payload)
            })
            .await?
            .1;
        let mut response: Value =
            serde_json::from_slice(&body).context("GraphQL endpoint did not return valid JSON")?;
        if let Some(errors) = response.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            let field = |name: &str| -> Vec<String> {
                errors.iter().filter_map(|e| e.get(name).and_then(Value::as_str)).map(str::to_string).collect()
            };
            return Err(GraphQlError { kinds: field("type"), messages: field("message") }.into());
        }
        match response.get_mut("data").map(Value::take) {
            Some(data) if !data.is_null() => Ok(data),
            _ => bail!("GraphQL response had no data: {}", truncate(&response.to_string(), 300)),
        }
    }

    /// Reports whether a git repository exists at `url`, by asking for its ref
    /// advertisement the way `git` would. Used for wikis, which GitHub only
    /// creates once a first page is written (the `has_wiki` flag just means
    /// the feature is switched on).
    pub async fn git_repository_exists(&self, url: &str) -> Result<bool> {
        let probe = format!("{url}/info/refs?service=git-upload-pack");
        let result = self
            .send(&probe, || {
                self.http.get(&probe).basic_auth("x-access-token", Some(&self.token)).timeout(Duration::from_secs(60))
            })
            .await;
        match result {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<ApiError>().is_some_and(|e| e.status == 404) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Streams `url` to the file at `dest`.
    ///
    /// The token is sent only when `authenticated` is set, and `reqwest` drops
    /// it again if the server redirects to another host (which is how GitHub
    /// serves release assets and attachments).
    pub async fn download(
        &self,
        url: &str,
        accept: Option<&str>,
        authenticated: bool,
        dest: &Path,
    ) -> Result<Downloaded> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut response = self
                .send(url, || {
                    let mut request = self.http.get(url);
                    if authenticated {
                        request = request.bearer_auth(&self.token);
                    }
                    if let Some(accept) = accept {
                        request = request.header(ACCEPT, accept);
                    }
                    request
                })
                .await?;
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);

            let mut file = tokio::fs::File::create(dest).await?;
            let mut size = 0u64;
            // Network errors are retried from the start; local write errors
            // (a full disk, say) are not going to get better and fail at once.
            let interrupted = loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        size += chunk.len() as u64;
                        file.write_all(&chunk).await.with_context(|| format!("cannot write {}", dest.display()))?;
                    }
                    Ok(None) => break None,
                    Err(e) => break Some(e),
                }
            };
            match interrupted {
                None => {
                    file.flush().await?;
                    return Ok(Downloaded { size, content_type });
                }
                Some(e) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(url, attempt, error = %e, "download interrupted; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Some(e) => return Err(e).with_context(|| format!("download of {url} kept failing")),
            }
        }
    }

    async fn get_api(&self, url: &str, accept: &str) -> Result<(HeaderMap, Vec<u8>)> {
        self.fetch_body(url, || {
            self.http.get(url).bearer_auth(&self.token).header(ACCEPT, accept).timeout(Duration::from_secs(90))
        })
        .await
    }

    /// Sends a request and reads the whole body, retrying if the body is cut short.
    async fn fetch_body(&self, url: &str, build: impl Fn() -> RequestBuilder) -> Result<(HeaderMap, Vec<u8>)> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self.send(url, &build).await?;
            let headers = response.headers().clone();
            match response.bytes().await {
                Ok(body) => return Ok((headers, body.to_vec())),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(url, attempt, error = %e, "response body interrupted; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(e).with_context(|| format!("reading the response from {url} kept failing")),
            }
        }
    }

    /// Sends a request until it succeeds, cannot succeed, or runs out of
    /// retries. Returns only 2xx responses; everything else is an error.
    async fn send(&self, url: &str, build: impl Fn() -> RequestBuilder) -> Result<Response> {
        let mut attempt = 0;
        let mut rate_limit_waits = 0;
        loop {
            attempt += 1;
            let result = {
                let _permit = self.permits.acquire().await?;
                self.wait_for_slot().await;
                self.requests.fetch_add(1, Ordering::Relaxed);
                build().send().await
            };

            let response = match result {
                Ok(response) => response,
                Err(e) if attempt < MAX_ATTEMPTS && !e.is_builder() => {
                    tracing::warn!(url, attempt, error = %e, "request failed; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("request to {url} kept failing")),
            };

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();

            if let Some(wait) = rate_limit_wait(status, &headers, &body) {
                rate_limit_waits += 1;
                if wait > self.max_rate_limit_wait || rate_limit_waits > MAX_RATE_LIMIT_WAITS {
                    bail!(
                        "GitHub rate limit hit at {url}; it resets in {}s, which is longer than this run will wait. \
                         The next scheduled run will pick up from here.",
                        wait.as_secs()
                    );
                }
                tracing::warn!(url, wait_seconds = wait.as_secs(), "rate limited by GitHub; waiting");
                tokio::time::sleep(wait).await;
                attempt -= 1; // waiting out a rate limit is not a failed attempt
                continue;
            }

            if status.is_server_error() && attempt < MAX_ATTEMPTS {
                tracing::warn!(url, attempt, status = status.as_u16(), "server error; retrying");
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }

            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| truncate(body.trim(), 300));
            return Err(ApiError { status: status.as_u16(), url: url.to_string(), message }.into());
        }
    }

    /// Spaces request starts at least [`MIN_INTERVAL`] apart.
    async fn wait_for_slot(&self) {
        let slot = {
            let mut next = self.next_slot.lock().await;
            let slot = (*next).max(Instant::now());
            *next = slot + MIN_INTERVAL;
            slot
        };
        tokio::time::sleep_until(slot).await;
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(5))
}

/// If a response is GitHub saying "slow down", returns how long to wait.
///
/// See <https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api>.
fn rate_limit_wait(status: StatusCode, headers: &HeaderMap, body: &str) -> Option<Duration> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let header =
        |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());

    if let Some(seconds) =
        headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds + 1));
    }
    if header("x-ratelimit-remaining") == Some(0)
        && let Some(reset) = header("x-ratelimit-reset")
    {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        return Some(Duration::from_secs(reset.saturating_sub(now) + 2));
    }
    if status == StatusCode::TOO_MANY_REQUESTS || body.to_ascii_lowercase().contains("secondary rate limit") {
        return Some(Duration::from_secs(60));
    }
    None
}

/// Where a paginated list continues, from the `Link` response header.
#[derive(Debug, Default, PartialEq)]
struct PageLinks {
    next: Option<String>,
    last: Option<String>,
}

impl PageLinks {
    fn parse(headers: &HeaderMap) -> Self {
        static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<([^<>]+)>\s*;\s*rel="(next|last)""#).unwrap());
        let mut links = Self::default();
        let Some(header) = headers.get(reqwest::header::LINK).and_then(|value| value.to_str().ok()) else {
            return links;
        };
        for captures in LINK.captures_iter(header) {
            let url = Some(captures[1].to_string());
            match &captures[2] {
                "next" => links.next = url,
                _ => links.last = url,
            }
        }
        links
    }

    /// Seen from page 1 of a list paged by number: the URLs of pages 2 to
    /// the last. `None` if the list is paged some other way (by cursor), has
    /// only one page, or this is not page 1.
    fn remaining_pages(&self) -> Option<Vec<String>> {
        // `[?&]` keeps this from matching the `page` in `per_page`.
        static PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([?&]page=)([0-9]+)").unwrap());
        let page_of = |url: &str| PAGE.captures(url).and_then(|captures| captures[2].parse::<u32>().ok());

        let (next, last) = (self.next.as_deref()?, self.last.as_deref()?);
        let last_page = page_of(last)?;
        if page_of(next)? != 2 || last_page < 2 {
            return None;
        }
        Some((2..=last_page).map(|page| PAGE.replace(last, format!("${{1}}{page}")).into_owned()).collect())
    }
}

/// Removes repeats from a list whose elements all carry an `id`.
///
/// A list that grows while it is being read shifts its contents across page
/// boundaries, which can deliver the same element twice.
fn dedup_by_id(items: Vec<Value>) -> Vec<Value> {
    let id = |item: &Value| item.get("id").filter(|id| id.is_number() || id.is_string()).map(Value::to_string);
    if !items.iter().all(|item| id(item).is_some()) {
        return items;
    }
    let mut seen = std::collections::HashSet::with_capacity(items.len());
    items.into_iter().filter(|item| seen.insert(id(item).expect("checked above"))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs.iter().map(|(name, value)| (name.parse().unwrap(), HeaderValue::from_str(value).unwrap())).collect()
    }

    #[test]
    fn page_links_are_read_from_the_link_header() {
        let middle = headers(&[(
            "link",
            r#"<https://api.github.com/repositories/1/issues/events?per_page=100&page=1>; rel="prev", <https://api.github.com/repositories/1/issues/events?per_page=100&page=3>; rel="next", <https://api.github.com/repositories/1/issues/events?per_page=100&page=89>; rel="last", <https://api.github.com/repositories/1/issues/events?per_page=100&page=1>; rel="first""#,
        )]);
        let links = PageLinks::parse(&middle);
        assert_eq!(
            links.next.as_deref(),
            Some("https://api.github.com/repositories/1/issues/events?per_page=100&page=3")
        );
        assert_eq!(
            links.last.as_deref(),
            Some("https://api.github.com/repositories/1/issues/events?per_page=100&page=89")
        );

        let end = headers(&[(
            "link",
            r#"<https://api.github.com/x?page=1>; rel="first", <https://api.github.com/x?page=2>; rel="prev""#,
        )]);
        assert_eq!(PageLinks::parse(&end), PageLinks::default());
        assert_eq!(PageLinks::parse(&HeaderMap::new()), PageLinks::default());
    }

    #[test]
    fn numbered_pages_are_enumerated_from_the_first_page() {
        let links = PageLinks {
            next: Some("https://api.github.com/repositories/1/pulls/comments?sort=created&per_page=100&page=2".into()),
            last: Some("https://api.github.com/repositories/1/pulls/comments?sort=created&per_page=100&page=4".into()),
        };
        assert_eq!(
            links.remaining_pages().unwrap(),
            [2, 3, 4].map(|n| format!(
                "https://api.github.com/repositories/1/pulls/comments?sort=created&per_page=100&page={n}"
            ))
        );

        // `page` first in the query string, and a `per_page` that must survive.
        let links = PageLinks {
            next: Some("https://api.github.com/x?page=2&per_page=100".into()),
            last: Some("https://api.github.com/x?page=3&per_page=100".into()),
        };
        assert_eq!(
            links.remaining_pages().unwrap(),
            ["https://api.github.com/x?page=2&per_page=100", "https://api.github.com/x?page=3&per_page=100"]
        );
    }

    #[test]
    fn other_kinds_of_paging_are_followed_link_by_link() {
        // Cursor paging: a next link, but no last and no page number.
        let cursor =
            PageLinks { next: Some("https://api.github.com/x?per_page=100&after=Y3Vyc29y".into()), last: None };
        assert_eq!(cursor.remaining_pages(), None);
        // A single page.
        assert_eq!(PageLinks::default().remaining_pages(), None);
        // Not standing on page 1.
        let middle = PageLinks {
            next: Some("https://api.github.com/x?per_page=100&page=3".into()),
            last: Some("https://api.github.com/x?per_page=100&page=9".into()),
        };
        assert_eq!(middle.remaining_pages(), None);
    }

    #[test]
    fn repeated_elements_are_dropped_only_when_everything_has_an_id() {
        let with_ids = vec![json!({"id": 1, "a": 1}), json!({"id": 2}), json!({"id": 1, "a": 1}), json!({"id": "x"})];
        assert_eq!(dedup_by_id(with_ids), vec![json!({"id": 1, "a": 1}), json!({"id": 2}), json!({"id": "x"})]);

        // Branches have no id; identical-looking entries must be left alone.
        let without = vec![json!({"name": "main"}), json!({"name": "main"})];
        assert_eq!(dedup_by_id(without.clone()), without);
        let mixed = vec![json!({"id": 1}), json!({"id": 1}), json!({"name": "x"})];
        assert_eq!(dedup_by_id(mixed.clone()), mixed);
    }

    #[test]
    fn rate_limits_are_told_apart_from_plain_refusals() {
        let forbidden = StatusCode::FORBIDDEN;

        // An ordinary permission error: not a rate limit.
        let h = headers(&[("x-ratelimit-remaining", "4000"), ("x-ratelimit-reset", "1")]);
        assert_eq!(rate_limit_wait(forbidden, &h, r#"{"message":"Must have admin rights to Repository."}"#), None);
        assert_eq!(rate_limit_wait(StatusCode::NOT_FOUND, &h, ""), None);

        // Secondary limit with Retry-After.
        let h = headers(&[("retry-after", "30")]);
        assert_eq!(rate_limit_wait(forbidden, &h, ""), Some(Duration::from_secs(31)));

        // Primary limit exhausted.
        let reset = chrono::Utc::now().timestamp() + 100;
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", &reset.to_string())]);
        let wait = rate_limit_wait(forbidden, &h, "").unwrap().as_secs();
        assert!((100..=103).contains(&wait), "{wait}");

        // Secondary limit with no headers at all.
        let body = r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes."}"#;
        assert_eq!(rate_limit_wait(forbidden, &HeaderMap::new(), body), Some(Duration::from_secs(60)));
        assert_eq!(
            rate_limit_wait(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), ""),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn only_permission_and_absence_count_as_unavailable() {
        let error = |status| anyhow::Error::from(ApiError { status, url: "u".into(), message: "m".into() });
        assert_eq!(unavailable_status(&error(403)), Some(403));
        assert_eq!(unavailable_status(&error(404)), Some(404));
        assert_eq!(unavailable_status(&error(410)), Some(410));
        assert_eq!(unavailable_status(&error(500)), None);
        assert_eq!(unavailable_status(&error(401)), None);
        assert_eq!(unavailable_status(&anyhow::anyhow!("network down")), None);
        // Context added on the way up must not hide the status.
        assert_eq!(unavailable_status(&error(404).context("fetching hooks")), Some(404));
    }
}
