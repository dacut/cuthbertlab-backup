//! Small helpers shared across modules.

use std::io::Write as _;

use {
    anyhow::Result,
    aws_sdk_s3::error::{ProvideErrorMetadata, SdkError},
    sha2::{Digest as _, Sha256},
};

/// Turns an AWS SDK error into a one-line one: `Code: message` when the
/// service answered, otherwise the chain of causes (a connection failure, a
/// timeout). The SDK's own formatting appends a dump of the whole response.
pub fn aws_error<E, R>(error: SdkError<E, R>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    match error.code() {
        Some(code) => anyhow::anyhow!("{code}: {}", error.message().unwrap_or("(no message)")),
        None => anyhow::Error::new(error),
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Gzips `data` deterministically (no timestamp), so that identical input
/// always yields identical output.
pub fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Shortens `text` to at most `max` characters, marking the cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Turns an arbitrary string (a tag, a file name, a URL path segment) into a
/// single safe S3 key segment: no `/`, no `.`/`..`, nothing outside a
/// conservative ASCII set. The mapping is percent-encoding, so it is reversible
/// and cannot make two different names collide (except by truncation, which
/// appends a hash of the full name).
pub fn key_segment(name: &str) -> String {
    const MAX: usize = 200;
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'@' | b'=' | b',') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    match out.as_str() {
        "" => return "_".into(),
        "." => return "%2E".into(),
        ".." => return "%2E%2E".into(),
        _ => {}
    }
    if out.len() > MAX {
        // Everything in `out` is ASCII, so byte slicing is safe.
        out.truncate(MAX);
        out.push('~');
        out.push_str(&sha256_hex(name.as_bytes())[..16]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_segments_are_safe() {
        assert_eq!(key_segment("v9.1.0"), "v9.1.0");
        assert_eq!(key_segment("music21-9.1.0-py3-none-any.whl"), "music21-9.1.0-py3-none-any.whl");
        assert_eq!(key_segment("release/2024"), "release%2F2024");
        assert_eq!(key_segment("a b"), "a%20b");
        assert_eq!(key_segment("100%"), "100%25");
        assert_eq!(key_segment("é"), "%C3%A9");
        assert_eq!(key_segment(""), "_");
        assert_eq!(key_segment("."), "%2E");
        assert_eq!(key_segment(".."), "%2E%2E");
        assert_ne!(key_segment("a/b"), key_segment("a%2Fb"));
    }

    #[test]
    fn long_key_segments_are_truncated_without_colliding() {
        let a = key_segment(&"x".repeat(500));
        let b = key_segment(&format!("{}y", "x".repeat(499)));
        assert!(a.len() < 230 && b.len() < 230);
        assert_ne!(a, b);
    }

    #[test]
    fn gzip_is_deterministic_and_reversible() {
        use std::io::Read as _;
        let data = b"[\n{\"a\":1}\n]\n".repeat(100);
        let first = gzip(&data).unwrap();
        assert_eq!(first, gzip(&data).unwrap());
        let mut round_trip = Vec::new();
        flate2::read::GzDecoder::new(&first[..]).read_to_end(&mut round_trip).unwrap();
        assert_eq!(round_trip, data);
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        assert_eq!(truncate("héllo", 10), "héllo");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
