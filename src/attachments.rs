//! Backs up images and files that people attached to issues, pull requests,
//! comments, discussions and release notes.
//!
//! GitHub does not expose attachments through its API. They are ordinary
//! uploads that the text merely links to, so the only way to find them is to
//! look for the links. Each one is stored at
//! `repos/<repo>/attachments/<host>/<path>`, and
//! `metadata/attachments.json.gz` maps every link to where it went.
//!
//! Uploads never change once made, so a link that has been stored is never
//! fetched again.

use std::{collections::BTreeSet, sync::LazyLock};

use {
    anyhow::Result,
    futures::StreamExt as _,
    regex::Regex,
    serde_json::{Value, json},
};

use crate::{
    Context,
    storage::Metadata,
    util::{key_segment, truncate},
};

/// Simultaneous downloads. Each one is buffered on local disk.
const CONCURRENCY: usize = 8;

/// Links to GitHub-hosted user uploads, across the URL schemes GitHub has used
/// over the years. Anything that can end a URL in Markdown, HTML or prose
/// stops the match.
static UPLOAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        https://
        (?:
            # 2023 onwards
            github\.com/user-attachments/(?:assets|files)/
          | # files, and 2023-era images, uploaded to a specific repository
            github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/(?:files|assets)/[0-9]+/
          | # 2016 to 2023
            user-images\.githubusercontent\.com/
          | # before 2016
            cloud\.githubusercontent\.com/assets/
          | f\.cloud\.github\.com/assets/
        )
        [^\s"'<>()\[\]{}\\|^`]+
        "#,
    )
    .unwrap()
});

/// The set of attachment links found in a repository's text.
#[derive(Default)]
pub struct Links(BTreeSet<String>);

impl Links {
    /// Collects links from every string anywhere inside `values`.
    pub fn scan(&mut self, values: &[Value]) {
        for value in values {
            self.scan_value(value);
        }
    }

    fn scan_value(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.scan_text(text),
            Value::Array(list) => list.iter().for_each(|value| self.scan_value(value)),
            Value::Object(map) => map.values().for_each(|value| self.scan_value(value)),
            _ => {}
        }
    }

    fn scan_text(&mut self, text: &str) {
        for found in UPLOAD.find_iter(text) {
            // Punctuation that ends a sentence rather than a URL.
            let url = found.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '_']);
            self.0.insert(url.to_string());
        }
    }
}

pub struct Outcome {
    /// One entry per link: `{"url", "key", "stored"}` plus `"error"` on failure.
    pub index: Vec<Value>,
    pub downloaded: usize,
    pub failed: usize,
    pub first_failure: Option<String>,
}

/// Downloads every link not already in storage.
///
/// A link that cannot be fetched is recorded, not fatal: uploads get deleted,
/// and a dead link in a ten-year-old comment must not fail the backup forever.
pub async fn backup(ctx: &Context, repo: &str, links: Links) -> Result<Outcome> {
    let dir = format!("repos/{repo}/attachments/");
    let stored = ctx.storage.list(&dir).await?;

    let results: Vec<(String, String, Result<bool>)> = futures::stream::iter(links.0)
        .map(|url| {
            let (dir, stored) = (&dir, &stored);
            async move {
                let key = format!("{dir}{}", key_for(&url));
                if stored.contains_key(&key) {
                    return (url, key, Ok(false));
                }
                let result = download(ctx, &url, &key).await.map(|()| true);
                (url, key, result)
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let mut outcome = Outcome { index: Vec::new(), downloaded: 0, failed: 0, first_failure: None };
    let mut results = results;
    results.sort_by(|a, b| a.0.cmp(&b.0));
    for (url, key, result) in results {
        match result {
            Ok(downloaded) => {
                outcome.downloaded += usize::from(downloaded);
                outcome.index.push(json!({ "url": url, "key": key, "stored": true }));
            }
            Err(e) => {
                let error = truncate(&format!("{e:#}"), 300);
                tracing::warn!(url, error, "attachment could not be downloaded");
                outcome.failed += 1;
                outcome.first_failure.get_or_insert_with(|| error.clone());
                outcome.index.push(json!({ "url": url, "key": key, "stored": false, "error": error }));
            }
        }
    }
    Ok(outcome)
}

async fn download(ctx: &Context, url: &str, key: &str) -> Result<()> {
    let scratch = tempfile::Builder::new().prefix("attachment-").tempfile_in(&ctx.config.work_dir)?;
    // Attachments on private repositories need authentication, but only ever
    // send the token to GitHub itself, never to a CDN host.
    let authenticated = url.starts_with(&format!("{}/", ctx.github.web_url()));
    let downloaded = ctx.github.download(url, None, authenticated, scratch.path()).await?;
    let content_type = downloaded.content_type.as_deref().unwrap_or("application/octet-stream");
    let metadata = Metadata::from([("source-url".to_string(), ascii_only(url))]);
    ctx.storage.put_file(key, scratch.path(), content_type, metadata).await?;
    Ok(())
}

/// Maps a link to a storage key (relative to the attachments directory) that
/// mirrors the URL: `<host>/<path...>`. Query strings are dropped; none of the
/// upload URL schemes use them to select content.
fn key_for(url: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    rest.split('/').filter(|segment| !segment.is_empty()).map(key_segment).collect::<Vec<_>>().join("/")
}

/// S3 user metadata must be ASCII.
fn ascii_only(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_graphic() { c } else { '?' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links_in(text: &str) -> Vec<String> {
        let mut links = Links::default();
        links.scan(&[json!({ "body": text, "nested": [{ "body": text }] })]);
        links.0.into_iter().collect()
    }

    #[test]
    fn finds_every_generation_of_upload_link() {
        let cases = [
            "https://github.com/user-attachments/assets/0f1e2d3c-4b5a-6978-8695-a4b3c2d1e0f9",
            "https://github.com/user-attachments/files/15912345/output.musicxml.zip",
            "https://github.com/cuthbertLab/music21/files/7654321/test.mxl.zip",
            "https://github.com/cuthbertLab/music21/assets/1234567/0f1e2d3c-4b5a-6978-8695-a4b3c2d1e0f9",
            "https://user-images.githubusercontent.com/1234567/98765432-0f1e2d00-4b5a-11eb-8695-a4b3c2d1e0f9.png",
            "https://cloud.githubusercontent.com/assets/1234567/7654321/0f1e2d3c-4b5a-11e5-8695-a4b3c2d1e0f9.png",
            "https://f.cloud.github.com/assets/1234567/765432/0f1e2d3c-4b5a-11e3-8695-a4b3c2d1e0f9.png",
        ];
        for url in cases {
            assert_eq!(links_in(&format!("before {url} after")), vec![url.to_string()], "{url}");
        }
    }

    #[test]
    fn links_end_where_markdown_html_and_prose_say_they_do() {
        let url = "https://user-images.githubusercontent.com/1/2-3.png";
        for text in [
            format!("![screenshot]({url})"),
            format!("![screenshot]({url} \"title\")"),
            format!("<img width=\"400\" src=\"{url}\">"),
            format!("<img src='{url}'/>"),
            format!("See {url}."),
            format!("See {url}, which shows it"),
            format!("[![x]({url})]({url})"),
            format!("line one\r\n{url}\r\nline three"),
            format!("**{url}**"),
        ] {
            assert_eq!(links_in(&text), vec![url.to_string()], "{text}");
        }
    }

    #[test]
    fn ordinary_links_are_left_alone() {
        for text in [
            "https://github.com/cuthbertLab/music21/issues/1234",
            "https://github.com/cuthbertLab/music21/blob/master/assets/1/logo.png",
            "https://raw.githubusercontent.com/cuthbertLab/music21/master/README.md",
            "https://avatars.githubusercontent.com/u/1234567?v=4",
            "https://example.com/user-attachments/assets/0f1e2d3c",
            "http://user-images.githubusercontent.com/1/2.png",
        ] {
            assert!(links_in(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn duplicate_links_are_collected_once() {
        let url = "https://github.com/user-attachments/assets/0f1e2d3c-4b5a-6978-8695-a4b3c2d1e0f9";
        assert_eq!(links_in(&format!("{url} and again {url}")).len(), 1);
    }

    #[test]
    fn keys_mirror_the_url_and_cannot_escape() {
        assert_eq!(
            key_for("https://user-images.githubusercontent.com/1234567/98765432-abc.png"),
            "user-images.githubusercontent.com/1234567/98765432-abc.png"
        );
        assert_eq!(
            key_for("https://github.com/cuthbertLab/music21/files/7654321/my%20score.mxl.zip?x=1#frag"),
            "github.com/cuthbertLab/music21/files/7654321/my%2520score.mxl.zip"
        );
        let hostile = key_for("https://github.com/user-attachments/files/1/../../../../etc/passwd");
        assert!(!hostile.split('/').any(|segment| segment == ".."), "{hostile}");
    }
}
