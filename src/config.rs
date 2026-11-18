//! Runtime configuration, read from environment variables.

use std::{path::PathBuf, time::Duration};

use {
    anyhow::{Context as _, Result, bail},
    aws_config::SdkConfig,
    serde_json::Value,
};

#[derive(Debug, Clone)]
pub struct Config {
    /// `GITHUB_ORG`: the organization to back up.
    pub org: String,
    /// `BACKUP_BUCKET`: destination S3 bucket.
    pub bucket: String,
    /// `BACKUP_PREFIX`: key prefix inside the bucket; empty or ending in `/`.
    pub prefix: String,
    /// `BACKUP_LOCAL_DIR` or `--local-dir`: write to a directory instead of S3.
    pub local_dir: Option<PathBuf>,
    /// `S3_FORCE_PATH_STYLE`: address the bucket as `host/bucket` rather than
    /// `bucket.host`, for S3-compatible stores. The endpoint itself comes from
    /// the standard `AWS_ENDPOINT_URL_S3`.
    pub s3_force_path_style: bool,
    /// `GITHUB_TOKEN_PARAMETER`: SSM SecureString parameter holding the token.
    pub token_parameter: Option<String>,
    /// `GITHUB_TOKEN`: the token itself. Takes precedence; meant for local runs.
    token: Option<String>,
    /// `GITHUB_API_URL`: REST endpoint (GraphQL is derived from it).
    pub api_url: String,
    /// `GITHUB_URL`: web and git endpoint.
    pub web_url: String,
    /// `BACKUP_PROJECTS`: whether to back up organization Projects (v2), which
    /// needs the `read:project` scope. Defaults to true so that a token without
    /// the scope fails loudly instead of silently skipping projects.
    pub backup_projects: bool,
    /// `BACKUP_ATTACHMENTS`: whether to download images and files attached to
    /// issues, pull requests, comments, discussions and release notes.
    pub backup_attachments: bool,
    /// `GITHUB_CONCURRENCY`: simultaneous requests to GitHub.
    pub concurrency: usize,
    /// `GITHUB_MAX_RATE_LIMIT_WAIT_SECONDS`: longest the function will sleep for
    /// a rate limit to reset before giving up (the retry then happens at the
    /// Step Functions level).
    pub max_rate_limit_wait: Duration,
    /// `WORK_DIR`: scratch space for clones and downloads.
    pub work_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let config = Self::read(false)?;
        config.validate()?;
        Ok(config)
    }

    /// Like [`Config::from_env`], but validation is deferred until command-line
    /// flags have been applied.
    pub fn from_env_for_cli() -> Result<Self> {
        Self::read(true)
    }

    fn read(lenient: bool) -> Result<Self> {
        let required = |name: &str| -> Result<String> {
            match var(name) {
                Some(value) => Ok(value),
                None if lenient => Ok(String::new()),
                None => bail!("environment variable {name} is required"),
            }
        };
        Ok(Self {
            org: required("GITHUB_ORG")?,
            bucket: required("BACKUP_BUCKET")?,
            prefix: normalize_prefix(&var("BACKUP_PREFIX").unwrap_or_default()),
            local_dir: var("BACKUP_LOCAL_DIR").map(PathBuf::from),
            s3_force_path_style: flag("S3_FORCE_PATH_STYLE", false)?,
            token_parameter: var("GITHUB_TOKEN_PARAMETER"),
            token: var("GITHUB_TOKEN"),
            api_url: var("GITHUB_API_URL")
                .unwrap_or_else(|| "https://api.github.com".into())
                .trim_end_matches('/')
                .to_string(),
            web_url: var("GITHUB_URL").unwrap_or_else(|| "https://github.com".into()).trim_end_matches('/').to_string(),
            backup_projects: flag("BACKUP_PROJECTS", true)?,
            backup_attachments: flag("BACKUP_ATTACHMENTS", true)?,
            concurrency: number("GITHUB_CONCURRENCY", 8)?.clamp(1, 32) as usize,
            max_rate_limit_wait: Duration::from_secs(number("GITHUB_MAX_RATE_LIMIT_WAIT_SECONDS", 300)?),
            work_dir: var("WORK_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.org.is_empty() {
            bail!("no organization given: set GITHUB_ORG or pass --org");
        }
        if !self.org.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            bail!("invalid organization name {:?}", self.org);
        }
        if self.local_dir.is_none() && self.bucket.is_empty() {
            bail!("no destination given: set BACKUP_BUCKET, or pass --bucket or --local-dir");
        }
        Ok(())
    }

    /// Returns the GitHub token, from `GITHUB_TOKEN` or else from the SSM
    /// parameter. The parameter may hold either the bare token or a JSON object
    /// with a `token` key.
    pub async fn resolve_token(&self, aws: &SdkConfig) -> Result<String> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        let Some(name) = &self.token_parameter else {
            bail!("no GitHub token: set GITHUB_TOKEN_PARAMETER (or GITHUB_TOKEN for local runs)");
        };
        let response = aws_sdk_ssm::Client::new(aws)
            .get_parameter()
            .name(name)
            .with_decryption(true)
            .send()
            .await
            .map_err(crate::util::aws_error)
            .with_context(|| format!("cannot read the GitHub token from SSM parameter {name}"))?;
        let raw = response
            .parameter()
            .and_then(|parameter| parameter.value())
            .with_context(|| format!("SSM parameter {name} has no value"))?
            .trim();
        parse_token(raw).with_context(|| format!("SSM parameter {name}"))
    }
}

fn parse_token(raw: &str) -> Result<String> {
    let token = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map
            .get("token")
            .and_then(Value::as_str)
            .context("holds a JSON object without a \"token\" key")?
            .trim()
            .to_string(),
        _ => raw.to_string(),
    };
    if token.is_empty() || token.contains(char::is_whitespace) {
        bail!("does not contain a usable token");
    }
    Ok(token)
}

/// Makes a key prefix either empty or `/`-terminated, with no leading `/`.
pub fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() { String::new() } else { format!("{trimmed}/") }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn flag(name: &str, default: bool) -> Result<bool> {
    match var(name).map(|v| v.to_ascii_lowercase()).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => bail!("{name} must be true or false, not {other:?}"),
    }
}

fn number(name: &str, default: u64) -> Result<u64> {
    match var(name) {
        None => Ok(default),
        Some(value) => value.parse().with_context(|| format!("{name} must be a non-negative integer, not {value:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, normalize_prefix, parse_token};

    #[tokio::test]
    #[ignore = "needs a local AWS emulator; run `make test-emulator`"]
    async fn emulator_token_is_read_from_ssm() {
        let aws = crate::emulator::aws().await;
        let ssm = aws_sdk_ssm::Client::new(&aws);
        let name = format!("/{}/github-token", crate::emulator::unique("config-test"));
        let mut config = Config::read(true).unwrap();
        config.token = None;
        config.token_parameter = Some(name.clone());

        let error = format!("{:#}", config.resolve_token(&aws).await.unwrap_err());
        assert!(error.contains(&name) && error.contains("ParameterNotFound"), "{error}");

        for (stored, expected) in [("ghp_bare", "ghp_bare"), (r#"{"token": "ghp_json"}"#, "ghp_json")] {
            ssm.put_parameter()
                .name(&name)
                .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
                .overwrite(true)
                .value(stored)
                .send()
                .await
                .unwrap();
            assert_eq!(config.resolve_token(&aws).await.unwrap(), expected);
        }
    }

    #[test]
    fn tokens_are_parsed() {
        assert_eq!(parse_token("ghp_abc").unwrap(), "ghp_abc");
        assert_eq!(parse_token(r#"{"token": " ghp_abc "}"#).unwrap(), "ghp_abc");
        for bad in ["", "two words", r#"{"key": "ghp_abc"}"#, r#"{"token": ""}"#] {
            assert!(parse_token(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn prefixes_are_normalized() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("github"), "github/");
        assert_eq!(normalize_prefix("/github/"), "github/");
        assert_eq!(normalize_prefix(" a/b "), "a/b/");
    }
}
