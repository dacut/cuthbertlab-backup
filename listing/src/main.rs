//! Directory listings for the backup website (see `terraform/website.tf`).
//!
//! CloudFront calls this function, through a Lambda function URL, for three
//! kinds of request:
//!
//! * A path ending in `/`: a listing of everything under that prefix, as
//!   HTML, or as JSON when asked for with `?Format=json` or
//!   `Accept: application/json`.
//! * `/_listing/style.css`: the listings' stylesheet. It is served from here
//!   because the site's Content-Security-Policy allows no inline styles.
//! * Any other path, but only once S3 has answered it with 403 or 404: the
//!   distribution's origin group then fails over to this function. A
//!   directory asked for without its trailing slash is redirected to it;
//!   anything else gets a "not found" page.
//!
//! Every link is a path on the same site, so the function does not need to
//! know the host name it is served under.

// The Lambda package is linked by Zig, whose warning about rustc's `-O1` is
// harmless; see the same attribute in the backup function's main.rs.
#![cfg_attr(all(target_os = "linux", target_env = "gnu"), allow(linker_messages))]

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    sync::Arc,
};

use {
    aws_sdk_s3::{
        error::{DisplayErrorContext, ProvideErrorMetadata as _},
        primitives::{DateTime, DateTimeFormat},
    },
    lambda_runtime::{Diagnostic, LambdaEvent, service_fn},
    percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode},
    serde::{Deserialize, Serialize},
    serde_json::{Value, json},
};

const STYLESHEET_PATH: &str = "/_listing/style.css";
const STYLESHEET: &str = include_str!("style.css");
const ALLOW: &str = "GET, HEAD, OPTIONS";

/// Characters left as they are in a path segment or query value: RFC 3986's
/// unreserved set. Everything else is percent-encoded, `%` included: the
/// backup writes keys containing a literal `%`.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// How long browsers and CloudFront may keep each kind of response.
const CACHE_LISTING: u32 = 300;
const CACHE_STATIC: u32 = 86_400;
const CACHE_REDIRECT: u32 = 3_600;
const CACHE_NOT_FOUND: u32 = 60;

/// The parts of a Lambda function URL event (payload format 2.0) used here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// As sent, still percent-encoded.
    raw_path: String,
    #[serde(default)]
    raw_query_string: String,
    /// Lower-case names; a repeated header's values are joined with commas.
    #[serde(default)]
    headers: HashMap<String, String>,
    request_context: RequestContext,
}

#[derive(Debug, Deserialize)]
struct RequestContext {
    http: HttpContext,
}

#[derive(Debug, Deserialize)]
struct HttpContext {
    method: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    status_code: u16,
    headers: BTreeMap<String, String>,
    body: String,
    is_base64_encoded: bool,
}

impl Response {
    fn new(status_code: u16, content_type: &str, cache: Cache, body: String) -> Self {
        let cache_control = match cache {
            Cache::For(seconds) => format!("public, max-age={seconds}"),
            Cache::Never => "no-store".to_string(),
        };
        let headers = BTreeMap::from([
            ("content-type".to_string(), content_type.to_string()),
            ("cache-control".to_string(), cache_control),
        ]);
        Self { status_code, headers, body, is_base64_encoded: false }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.insert(name.to_string(), value.into());
        self
    }
}

enum Cache {
    For(u32),
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    Html,
    Json,
}

impl Format {
    /// `?Format=` wins; then whichever of HTML and JSON the Accept header
    /// names first; then HTML, which is what browsers and `curl` both get.
    fn choose(query: Option<&str>, accept: Option<&str>) -> Self {
        if let Some(format) = query {
            return if format.eq_ignore_ascii_case("json") || format == "application/json" {
                Format::Json
            } else {
                Format::Html
            };
        }
        for media_type in accept.unwrap_or_default().split(',') {
            match media_type.split(';').next().unwrap_or_default().trim() {
                "application/json" => return Format::Json,
                "text/html" => return Format::Html,
                _ => {}
            }
        }
        Format::Html
    }
}

pub struct Site {
    client: aws_sdk_s3::Client,
    bucket: String,
    /// Shown at the top of every page.
    title: String,
    /// Entries per page of a listing; S3 allows at most 1,000.
    page_size: i32,
}

impl Site {
    fn from_env(client: aws_sdk_s3::Client) -> Result<Self, String> {
        let bucket = std::env::var("BUCKET").map_err(|_| "environment variable BUCKET is required".to_string())?;
        let title = std::env::var("SITE_TITLE").ok().filter(|title| !title.trim().is_empty());
        Ok(Self { client, title: title.unwrap_or_else(|| bucket.clone()), bucket, page_size: 1000 })
    }
}

/// One page of the entries directly under a prefix.
struct Listing {
    prefix: String,
    /// Full prefixes, each ending in `/`.
    directories: Vec<String>,
    files: Vec<File>,
    /// Where the next page starts, if there is one.
    next: Option<String>,
}

struct File {
    key: String,
    size: i64,
    last_modified: Option<DateTime>,
    etag: Option<String>,
}

impl Listing {
    fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.files.is_empty()
    }

    fn name<'a>(&self, key: &'a str) -> &'a str {
        key.strip_prefix(self.prefix.as_str()).unwrap_or(key)
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_runtime::Error> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_current_span(false)
        .without_time()
        .init();

    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let site = Arc::new(Site::from_env(aws_sdk_s3::Client::new(&aws))?);
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Request>| {
        let site = Arc::clone(&site);
        async move { Ok::<_, Diagnostic>(handle(&site, &event.payload).await) }
    }))
    .await
}

pub async fn handle(site: &Site, request: &Request) -> Response {
    let method = request.request_context.http.method.as_str();
    let mut response = match method {
        "GET" | "HEAD" => route(site, request).await,
        "OPTIONS" => Response::new(204, "text/plain; charset=utf-8", Cache::For(CACHE_STATIC), String::new())
            .header("allow", ALLOW),
        _ => Response::new(405, "text/plain; charset=utf-8", Cache::Never, "Method not allowed\n".into())
            .header("allow", ALLOW),
    };
    tracing::info!(method, path = request.raw_path, status = response.status_code, "handled request");
    if method == "HEAD" {
        response.body.clear();
    }
    response
}

async fn route(site: &Site, request: &Request) -> Response {
    if request.raw_path == STYLESHEET_PATH {
        return Response::new(200, "text/css; charset=utf-8", Cache::For(CACHE_STATIC), STYLESHEET.into());
    }

    let query = parse_query(&request.raw_query_string);
    let format =
        Format::choose(query.get("Format").map(String::as_str), request.headers.get("accept").map(String::as_str));
    let Some(path) = decode_path(&request.raw_path) else {
        return error_page(site, format, 400, "Bad request", "That is not a valid path.", "");
    };

    if path.is_empty() || path.ends_with('/') {
        let token = query.get("ContinuationToken").map(String::as_str);
        match list(site, format, &path, token, site.page_size).await {
            // Nothing under a prefix means there is no such directory; only
            // the root of the site exists even when empty.
            Ok(listing) if listing.is_empty() && !path.is_empty() && token.is_none() => not_found(site, format, &path),
            Ok(listing) => {
                let response = match format {
                    Format::Html => Response::new(
                        200,
                        "text/html; charset=utf-8",
                        Cache::For(CACHE_LISTING),
                        render_html(site, &listing),
                    ),
                    Format::Json => Response::new(
                        200,
                        "application/json",
                        Cache::For(CACHE_LISTING),
                        format!("{:#}\n", render_json(&listing)),
                    ),
                };
                response.header("vary", "Accept")
            }
            Err(response) => response,
        }
    } else {
        // S3 has no object here. If it is a directory named without its
        // trailing slash, send the browser to the directory.
        match list(site, format, &format!("{path}/"), None, 1).await {
            Ok(listing) if !listing.is_empty() => {
                let location = format!("{}/", href(&path));
                let body =
                    format!("<!DOCTYPE html>\n<title>Moved</title>\n<a href=\"{0}\">{0}</a>\n", escape(&location));
                Response::new(301, "text/html; charset=utf-8", Cache::For(CACHE_REDIRECT), body)
                    .header("location", location)
            }
            Ok(_) => not_found(site, format, &path),
            Err(response) => response,
        }
    }
}

async fn list(site: &Site, format: Format, prefix: &str, token: Option<&str>, max: i32) -> Result<Listing, Response> {
    let mut request = site.client.list_objects_v2().bucket(&site.bucket).prefix(prefix).delimiter("/").max_keys(max);
    if let Some(token) = token {
        request = request.continuation_token(token);
    }
    match request.send().await {
        Ok(page) => {
            let next = match page.is_truncated() {
                Some(true) => page.next_continuation_token().map(str::to_string),
                _ => None,
            };
            Ok(Listing {
                prefix: prefix.to_string(),
                directories: page.common_prefixes().iter().filter_map(|p| p.prefix()).map(str::to_string).collect(),
                files: page
                    .contents()
                    .iter()
                    .filter_map(|object| {
                        let key = object.key()?;
                        // A zero-length "folder" object for the prefix itself.
                        (key != prefix).then(|| File {
                            key: key.to_string(),
                            size: object.size().unwrap_or(0),
                            last_modified: object.last_modified().cloned(),
                            etag: object.e_tag().map(str::to_string),
                        })
                    })
                    .collect(),
                next,
            })
        }
        Err(e) if token.is_some() && e.code() == Some("InvalidArgument") => Err(error_page(
            site,
            format,
            400,
            "Bad request",
            "That page of the listing does not exist. Start again from the first page.",
            prefix,
        )),
        Err(e) => {
            tracing::error!(prefix, error = %DisplayErrorContext(&e), "cannot list the bucket");
            Err(error_page(site, format, 502, "Listing failed", "This directory cannot be listed right now.", prefix))
        }
    }
}

fn not_found(site: &Site, format: Format, path: &str) -> Response {
    let message = format!("There is nothing at /{path}.");
    error_page(site, format, 404, "Not found", &message, parent_of(path))
}

/// An error, as a page with a link to `back` (a directory prefix), or as JSON.
fn error_page(site: &Site, format: Format, status: u16, title: &str, message: &str, back: &str) -> Response {
    let cache = if status >= 500 { Cache::Never } else { Cache::For(CACHE_NOT_FOUND) };
    match format {
        Format::Json => {
            Response::new(status, "application/json", cache, format!("{:#}\n", json!({ "Error": message })))
        }
        Format::Html => {
            let mut out = String::new();
            page_start(&mut out, site, title);
            let _ = write!(
                out,
                "<h1>{}</h1>\n<p>{}</p>\n<p><a href=\"{}\">Back to /{}</a></p>\n",
                escape(title),
                escape(message),
                escape(&href(back)),
                escape(back)
            );
            page_end(&mut out);
            Response::new(status, "text/html; charset=utf-8", cache, out)
        }
    }
}

fn render_html(site: &Site, listing: &Listing) -> String {
    let mut out = String::new();
    page_start(&mut out, site, &format!("/{}", listing.prefix));

    out.push_str("<h1>Index of <a href=\"/\">/</a>");
    let mut so_far = String::new();
    for segment in listing.prefix.split_terminator('/') {
        so_far.push_str(segment);
        so_far.push('/');
        let _ = write!(out, "<a href=\"{}\">{}</a>/", escape(&href(&so_far)), escape(segment));
    }
    out.push_str("</h1>\n");

    out.push_str(concat!(
        "<table>\n<thead><tr><th class=\"name\">Name</th><th class=\"size\">Size</th>",
        "<th class=\"date\">Last modified (UTC)</th></tr></thead>\n<tbody>\n"
    ));
    if !listing.prefix.is_empty() {
        let _ = writeln!(
            out,
            "<tr class=\"up\"><td class=\"name\"><a href=\"{}\">../</a></td><td class=\"size\"></td><td class=\"date\"></td></tr>",
            escape(&href(parent_of(&listing.prefix)))
        );
    }
    for directory in &listing.directories {
        let _ = writeln!(
            out,
            "<tr class=\"dir\"><td class=\"name\"><a href=\"{}\">{}</a></td><td class=\"size\"></td><td class=\"date\"></td></tr>",
            escape(&href(directory)),
            escape(listing.name(directory))
        );
    }
    for file in &listing.files {
        let modified = file.last_modified.as_ref().map(display_time).unwrap_or_default();
        let _ = writeln!(
            out,
            "<tr class=\"file\"><td class=\"name\"><a href=\"{}\">{}</a></td><td class=\"size\" title=\"{} bytes\">{}</td><td class=\"date\">{}</td></tr>",
            escape(&href(&file.key)),
            escape(listing.name(&file.key)),
            file.size,
            human_size(file.size),
            escape(&modified)
        );
    }
    out.push_str("</tbody>\n</table>\n");

    if listing.is_empty() {
        out.push_str("<p class=\"note\">This directory is empty.</p>\n");
    }
    if let Some(token) = &listing.next {
        let _ = writeln!(
            out,
            "<p class=\"more\"><a href=\"?ContinuationToken={}\">Next page</a></p>",
            escape(&utf8_percent_encode(token, UNRESERVED).to_string())
        );
    }
    page_end(&mut out);
    out
}

fn render_json(listing: &Listing) -> Value {
    json!({
        "Path": href(&listing.prefix),
        "Subdirectories": listing.directories.iter().map(|directory| json!({
            "Name": listing.name(directory),
            "Path": href(directory),
        })).collect::<Vec<_>>(),
        "Files": listing.files.iter().map(|file| json!({
            "Name": listing.name(&file.key),
            "Path": href(&file.key),
            "Size": file.size,
            "LastModified": file.last_modified.as_ref().and_then(|t| t.fmt(DateTimeFormat::DateTime).ok()),
            "ETag": file.etag,
        })).collect::<Vec<_>>(),
        "IsTruncated": listing.next.is_some(),
        "NextContinuationToken": listing.next,
    })
}

fn page_start(out: &mut String, site: &Site, heading: &str) {
    let _ = write!(
        out,
        concat!(
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n",
            "<title>{} · {}</title>\n<link rel=\"stylesheet\" href=\"{}\">\n</head>\n<body>\n",
            "<header><a href=\"/\">{}</a></header>\n<main>\n"
        ),
        escape(heading),
        escape(&site.title),
        STYLESHEET_PATH,
        escape(&site.title)
    );
}

fn page_end(out: &mut String) {
    out.push_str("</main>\n</body>\n</html>\n");
}

/// The key, or key prefix, that a request path names: the path without its
/// leading slash, percent-decoded. `None` if that is not UTF-8.
fn decode_path(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('/')?;
    percent_decode_str(rest).decode_utf8().ok().map(Cow::into_owned)
}

/// The site path for a key or key prefix: the inverse of [`decode_path`].
fn href(key: &str) -> String {
    let encoded: Vec<String> =
        key.split('/').map(|segment| utf8_percent_encode(segment, UNRESERVED).to_string()).collect();
    format!("/{}", encoded.join("/"))
}

/// The directory containing `path` (a key, or a prefix ending in `/`), as a
/// prefix ending in `/`, or `""` for the root.
fn parent_of(path: &str) -> &str {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    match trimmed.rfind('/') {
        Some(slash) => &trimmed[..=slash],
        None => "",
    }
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    fn decode(component: &str) -> Option<String> {
        percent_decode_str(&component.replace('+', " ")).decode_utf8().ok().map(Cow::into_owned)
    }
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(name)?, decode(value)?))
        })
        .collect()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `2026-11-18 21:56`, in UTC.
fn display_time(time: &DateTime) -> String {
    let iso = time.fmt(DateTimeFormat::DateTime).unwrap_or_default();
    iso.get(..16).map(|minutes| minutes.replace('T', " ")).unwrap_or(iso)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline_site() -> Site {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        Site {
            client: aws_sdk_s3::Client::from_conf(config),
            bucket: "b".into(),
            title: "Test <site>".into(),
            page_size: 1000,
        }
    }

    /// A request as a Lambda function URL delivers it.
    fn request(method: &str, raw_path: &str, query: &str, accept: Option<&str>) -> Request {
        let mut headers = serde_json::Map::new();
        headers.insert("host".into(), "abc.lambda-url.us-west-2.on.aws".into());
        if let Some(accept) = accept {
            headers.insert("accept".into(), accept.into());
        }
        serde_json::from_value(json!({
            "version": "2.0",
            "routeKey": "$default",
            "rawPath": raw_path,
            "rawQueryString": query,
            "headers": headers,
            "requestContext": {
                "domainName": "abc.lambda-url.us-west-2.on.aws",
                "http": { "method": method, "path": raw_path, "protocol": "HTTP/1.1", "sourceIp": "192.0.2.1" },
            },
            "isBase64Encoded": false,
        }))
        .unwrap()
    }

    fn listing() -> Listing {
        Listing {
            prefix: "github/repos/".into(),
            directories: vec!["github/repos/music21/".into(), "github/repos/<script>/".into()],
            files: vec![File {
                key: "github/repos/my%20score.zip".into(),
                size: 1_500_000,
                last_modified: Some(DateTime::from_secs(1_763_502_960)),
                etag: Some("\"abc\"".into()),
            }],
            next: Some("a+b/c=".into()),
        }
    }

    #[test]
    fn paths_and_links_are_inverse() {
        // Keys contain a literal "%": the link must encode it, or S3 would
        // decode "%20" into a space and look for a different key.
        assert_eq!(href("github/repos/my%20score.zip"), "/github/repos/my%2520score.zip");
        assert_eq!(decode_path("/github/repos/my%2520score.zip").unwrap(), "github/repos/my%20score.zip");
        assert_eq!(href("a b+c/"), "/a%20b%2Bc/");
        assert_eq!(href(""), "/");
        assert_eq!(href("github/"), "/github/");
        for key in ["", "a/", "a/b.txt", "x/100% done/é+ü=?.zip"] {
            assert_eq!(decode_path(&href(key)).unwrap(), key);
        }
        assert!(decode_path("/%FF").is_none());
    }

    #[test]
    fn parents() {
        assert_eq!(parent_of("a/b/c.txt"), "a/b/");
        assert_eq!(parent_of("a/b/"), "a/");
        assert_eq!(parent_of("a/"), "");
        assert_eq!(parent_of("a"), "");
        assert_eq!(parent_of(""), "");
    }

    #[test]
    fn query_strings_are_decoded() {
        let query = parse_query("Format=json&ContinuationToken=a%2Bb%2Fc%3D&flag&x=a+b");
        assert_eq!(query["Format"], "json");
        assert_eq!(query["ContinuationToken"], "a+b/c=");
        assert_eq!(query["flag"], "");
        assert_eq!(query["x"], "a b");
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn format_is_negotiated() {
        assert_eq!(Format::choose(Some("json"), Some("text/html")), Format::Json);
        assert_eq!(Format::choose(Some("html"), Some("application/json")), Format::Html);
        assert_eq!(Format::choose(None, Some("application/json")), Format::Json);
        assert_eq!(Format::choose(None, Some("text/html,application/xhtml+xml,*/*;q=0.8")), Format::Html);
        assert_eq!(Format::choose(None, Some("application/json;q=0.9, text/plain")), Format::Json);
        assert_eq!(Format::choose(None, Some("*/*")), Format::Html);
        assert_eq!(Format::choose(None, None), Format::Html);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(590_106_893), "562.8 MiB");
        assert_eq!(human_size(1_288_490_189), "1.2 GiB");
    }

    #[test]
    fn html_listings_escape_link_and_paginate() {
        let html = render_html(&offline_site(), &listing());
        assert!(html.contains("<title>/github/repos/ · Test &lt;site&gt;</title>"), "{html}");
        assert!(html.contains(r#"<link rel="stylesheet" href="/_listing/style.css">"#));
        assert!(html.contains(r#"<header><a href="/">Test &lt;site&gt;</a></header>"#));
        assert!(html.contains(
            r#"Index of <a href="/">/</a><a href="/github/">github</a>/<a href="/github/repos/">repos</a>/"#
        ));
        assert!(html.contains(r#"<a href="/github/">../</a>"#));
        assert!(html.contains(r#"<a href="/github/repos/music21/">music21/</a>"#));
        assert!(!html.contains("<script>"));
        assert!(html.contains(r#"<a href="/github/repos/%3Cscript%3E/">&lt;script&gt;/</a>"#));
        assert!(html.contains(r#"<a href="/github/repos/my%2520score.zip">my%20score.zip</a>"#));
        assert!(html.contains(r#"title="1500000 bytes">1.4 MiB<"#));
        assert!(html.contains(">2025-11-18 21:56<"), "{html}");
        assert!(html.contains(r#"<a href="?ContinuationToken=a%2Bb%2Fc%3D">Next page</a>"#));
    }

    #[test]
    fn json_listings_carry_paths_and_the_next_page() {
        let value = render_json(&listing());
        assert_eq!(value["Path"], "/github/repos/");
        assert_eq!(value["Subdirectories"][0], json!({ "Name": "music21/", "Path": "/github/repos/music21/" }));
        assert_eq!(value["Files"][0]["Name"], "my%20score.zip");
        assert_eq!(value["Files"][0]["Path"], "/github/repos/my%2520score.zip");
        assert_eq!(value["Files"][0]["Size"], 1_500_000);
        assert_eq!(value["Files"][0]["LastModified"], "2025-11-18T21:56:00Z");
        assert_eq!(value["IsTruncated"], true);
        assert_eq!(value["NextContinuationToken"], "a+b/c=");
    }

    #[tokio::test]
    async fn requests_that_need_no_bucket() {
        let site = offline_site();

        let css = handle(&site, &request("GET", "/_listing/style.css", "", None)).await;
        assert_eq!(css.status_code, 200);
        assert_eq!(css.headers["content-type"], "text/css; charset=utf-8");
        assert!(css.body.contains("prefers-color-scheme"));

        let head = handle(&site, &request("HEAD", "/_listing/style.css", "", None)).await;
        assert_eq!(head.status_code, 200);
        assert!(head.body.is_empty());
        assert_eq!(head.headers["content-type"], "text/css; charset=utf-8");

        let options = handle(&site, &request("OPTIONS", "/github/", "", None)).await;
        assert_eq!(options.status_code, 204);
        assert_eq!(options.headers["allow"], ALLOW);

        let post = handle(&site, &request("POST", "/github/", "", None)).await;
        assert_eq!(post.status_code, 405);

        let invalid = handle(&site, &request("GET", "/%FF/", "", None)).await;
        assert_eq!(invalid.status_code, 400);

        // The shape Lambda expects back from a function URL.
        let wire = serde_json::to_value(&css).unwrap();
        assert_eq!(wire["statusCode"], 200);
        assert_eq!(wire["isBase64Encoded"], false);
        assert_eq!(wire["headers"]["cache-control"], "public, max-age=86400");
    }

    /// Runs against Moto (`make test-emulator`); refuses anything else, so it
    /// cannot create a bucket in a real account.
    #[tokio::test]
    #[ignore = "needs a local AWS emulator; run `make test-emulator`"]
    async fn emulator_listings_redirects_and_not_found() {
        let endpoint = std::env::var("AWS_ENDPOINT_URL").unwrap_or_default();
        assert!(
            endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://localhost:"),
            "emulator tests need AWS_ENDPOINT_URL to point at a local emulator, not {endpoint:?}"
        );
        let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client =
            aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&aws).force_path_style(true).build());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let bucket = format!("listing-test-{}-{nanos}", std::process::id());
        client.create_bucket().bucket(&bucket).send().await.unwrap();
        for key in [
            "github/org/org.json",
            "github/repos/m/git/m.bundle",
            "github/repos/m/manifest.json",
            "github/repos/m/attachments/x/my%20score.zip",
            "github/repos/n/manifest.json",
        ] {
            client.put_object().bucket(&bucket).key(key).body(b"data".to_vec().into()).send().await.unwrap();
        }
        let site = Site { client, bucket, title: "Backups".into(), page_size: 2 };

        let root = handle(&site, &request("GET", "/", "", Some("text/html"))).await;
        assert_eq!(root.status_code, 200);
        assert!(root.body.contains(r#"<a href="/github/">github/</a>"#), "{}", root.body);
        assert!(!root.body.contains("../"));

        // Every entry, across pages of two.
        let (mut names, mut pages, mut query) = (Vec::new(), 0, String::from("Format=json"));
        loop {
            let response = handle(&site, &request("GET", "/github/repos/m/", &query, None)).await;
            assert_eq!(response.status_code, 200, "{}", response.body);
            assert_eq!(response.headers["content-type"], "application/json");
            let page: Value = serde_json::from_str(&response.body).unwrap();
            for entry in page["Subdirectories"].as_array().unwrap().iter().chain(page["Files"].as_array().unwrap()) {
                names.push(entry["Name"].as_str().unwrap().to_string());
            }
            pages += 1;
            match page["NextContinuationToken"].as_str() {
                Some(token) => {
                    query = format!("Format=json&ContinuationToken={}", utf8_percent_encode(token, UNRESERVED))
                }
                None => break,
            }
        }
        names.sort();
        assert_eq!(names, ["attachments/", "git/", "manifest.json"]);
        assert!(pages > 1);

        let attachments = handle(&site, &request("GET", "/github/repos/m/attachments/x/", "", None)).await;
        assert!(attachments.body.contains(r#"href="/github/repos/m/attachments/x/my%2520score.zip""#));

        let redirect = handle(&site, &request("GET", "/github/repos/m", "", None)).await;
        assert_eq!(redirect.status_code, 301);
        assert_eq!(redirect.headers["location"], "/github/repos/m/");

        for path in ["/github/repos/missing", "/github/repos/missing/", "/github/repos/m/manifest.json/"] {
            let missing = handle(&site, &request("GET", path, "", None)).await;
            assert_eq!(missing.status_code, 404, "{path}");
            assert!(
                missing.body.contains(r#"Back to /github/repos/"#) || path.contains("manifest"),
                "{}",
                missing.body
            );
        }

        let head = handle(&site, &request("HEAD", "/github/", "", None)).await;
        assert_eq!(head.status_code, 200);
        assert!(head.body.is_empty());
    }
}
