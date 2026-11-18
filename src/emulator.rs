//! Support for the tests that talk to AWS APIs.
//!
//! Those tests run against a local emulator ([Moto]) and are skipped by a
//! plain `cargo test`. `make test-emulator` starts the emulator and runs them.
//!
//! An emulator is only as strict as its authors made it. Moto accepts some
//! requests that AWS rejects (it let through a multipart completion that S3
//! answers with `InvalidPart`), so these tests show that requests are
//! well-formed and that data round-trips, not that AWS will accept them.
//!
//! [Moto]: https://github.com/getmoto/moto

use aws_config::{BehaviorVersion, SdkConfig};

/// AWS configuration for a test, taken from the environment as usual.
///
/// Panics unless `AWS_ENDPOINT_URL` points at this machine: the tests create
/// buckets and parameters, and must never do that in a real account because
/// someone ran `cargo test -- --ignored` with their own credentials.
pub async fn aws() -> SdkConfig {
    let endpoint = std::env::var("AWS_ENDPOINT_URL").unwrap_or_default();
    assert!(
        endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://localhost:"),
        "emulator tests need AWS_ENDPOINT_URL to point at a local emulator, not {endpoint:?}; run `make test-emulator`"
    );
    aws_config::load_defaults(BehaviorVersion::latest()).await
}

/// A name no other test, or earlier run against the same emulator, has used.
/// Valid as a bucket name and as an SSM parameter path segment.
pub fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}
