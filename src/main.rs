//! Backs up every repository in a GitHub organization to S3.
//!
//! The same binary runs in two modes:
//!
//! * **AWS Lambda** (when `AWS_LAMBDA_RUNTIME_API` is set): each invocation
//!   handles one [`event::Request`]. A Step Functions state machine calls
//!   `plan` once and then fans the returned tasks out, one invocation each.
//! * **Command line** (otherwise): for running a backup, or a single task,
//!   from a workstation. See `--help`.

// The Lambda package is linked by Zig (through cargo-lambda), which prints
// `ignoring deprecated linker optimization setting "1"` about the `-O1` that
// rustc passes to every GNU-style linker for an optimized build. rustc repeats
// whatever the linker prints as a warning. The flag is harmless and neither
// tool offers a way to drop it, so the repeat is silenced for that target only.
#![cfg_attr(all(target_os = "linux", target_env = "gnu"), allow(linker_messages))]

mod attachments;
mod config;
#[cfg(test)]
mod emulator;
mod event;
mod git;
mod github;
mod graphql;
mod metadata;
mod org;
mod plan;
mod releases;
mod storage;
mod util;

use {
    crate::{
        config::Config,
        event::{Request, Task, TaskResult},
        github::GitHub,
        storage::Storage,
    },
    anyhow::{Context as _, Result},
    clap::{Parser, Subcommand},
    lambda_runtime::{Diagnostic, LambdaEvent, service_fn},
    serde_json::Value,
    std::{path::PathBuf, sync::Arc},
};

/// Everything a task needs to run.
pub struct Context {
    pub config: Config,
    pub github: GitHub,
    pub storage: Storage,
}

impl Context {
    async fn new(config: Config) -> Result<Self> {
        let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let token = config.resolve_token(&aws).await?;
        let github = GitHub::new(&config, token)?;
        let storage = Storage::new(&config, &aws)?;
        Ok(Self { config, github, storage })
    }

    /// Runs one backup task to completion.
    pub async fn run_task(&self, task: &Task) -> Result<TaskResult> {
        task.validate()?;
        let started = std::time::Instant::now();
        let requests_before = self.github.request_count();
        tracing::info!(task = %task, "starting task");

        let outcome = match task {
            Task::Org => org::backup(self).await,
            Task::Git { repo } => git::backup(self, repo).await,
            Task::Metadata { repo } => metadata::backup(self, repo).await,
        };

        let elapsed = started.elapsed().as_secs_f64();
        let requests = self.github.request_count() - requests_before;
        match outcome {
            Ok(mut result) => {
                result.elapsed_seconds = (elapsed * 10.0).round() / 10.0;
                result.github_requests = requests;
                tracing::info!(task = %task, elapsed, requests, warnings = result.warnings.len(), "task succeeded");
                Ok(result)
            }
            Err(e) => {
                tracing::error!(task = %task, elapsed, requests, error = format!("{e:#}"), "task failed");
                Err(e.context(format!("task {task} failed")))
            }
        }
    }
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// GitHub organization to back up [env: GITHUB_ORG]
    #[arg(long, global = true)]
    org: Option<String>,

    /// S3 bucket to write to [env: BACKUP_BUCKET]
    #[arg(long, global = true)]
    bucket: Option<String>,

    /// Key prefix inside the bucket [env: BACKUP_PREFIX]
    #[arg(long, global = true)]
    prefix: Option<String>,

    /// Write to this local directory instead of S3 (for testing and one-off exports)
    #[arg(long, global = true, conflicts_with = "bucket")]
    local_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the tasks a full backup consists of
    Plan,
    /// Back up organization-level data (members, teams, projects, repository index)
    Org,
    /// Back up one repository's git data: every ref, plus the wiki
    Git { repo: String },
    /// Back up one repository's issues, pull requests, discussions, releases, attachments, ...
    Metadata { repo: String },
    /// Plan and then run every task in sequence
    All,
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("AWS_LAMBDA_RUNTIME_API").is_some() { run_lambda().await } else { run_cli().await }
}

async fn run_lambda() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(log_filter())
        .with_current_span(false)
        .without_time() // CloudWatch adds its own timestamps
        .init();

    let context = Arc::new(Context::new(Config::from_env()?).await?);
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| {
        let context = context.clone();
        async move { handle(&context, event.payload).await }
    }))
    .await
    .map_err(|e| anyhow::anyhow!("Lambda runtime error: {e}"))
}

/// Handles a single Lambda invocation.
///
/// Failures are reported with a stable error type and the full chain of
/// causes as the message, which is what ends up in the state machine's
/// execution history and in the alert email.
async fn handle(context: &Context, payload: Value) -> Result<Value, Diagnostic> {
    let result: Result<Value> = async {
        let request: Request =
            serde_json::from_value(payload.clone()).with_context(|| format!("unrecognized event: {payload}"))?;
        Ok(match request {
            Request::Plan => serde_json::to_value(plan::plan(context).await?)?,
            Request::Backup(task) => serde_json::to_value(context.run_task(&task).await?)?,
        })
    }
    .await;
    result.map_err(|e| Diagnostic { error_type: "BackupError".into(), error_message: format!("{e:#}") })
}

async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let mut config = Config::from_env_for_cli()?;
    if let Some(org) = cli.org {
        config.org = org;
    }
    if let Some(bucket) = cli.bucket {
        config.bucket = bucket;
    }
    if let Some(prefix) = cli.prefix {
        config.prefix = config::normalize_prefix(&prefix);
    }
    if cli.local_dir.is_some() {
        config.local_dir = cli.local_dir;
    }
    config.validate()?;

    let context = Context::new(config).await?;
    let output = match cli.command {
        Command::Plan => serde_json::to_value(plan::plan(&context).await?)?,
        Command::Org => serde_json::to_value(context.run_task(&Task::Org).await?)?,
        Command::Git { repo } => serde_json::to_value(context.run_task(&Task::Git { repo }).await?)?,
        Command::Metadata { repo } => serde_json::to_value(context.run_task(&Task::Metadata { repo }).await?)?,
        Command::All => run_all(&context).await?,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Runs every planned task in sequence, carrying on past failures so that one
/// broken repository does not stop the rest from being backed up.
async fn run_all(context: &Context) -> Result<Value> {
    let plan = plan::plan(context).await?;
    let mut results = Vec::new();
    let mut failed = 0;
    for request in &plan.tasks {
        let Request::Backup(task) = request else { continue };
        match context.run_task(task).await {
            Ok(result) => results.push(serde_json::to_value(result)?),
            Err(e) => {
                failed += 1;
                results.push(serde_json::json!({
                    "task": task,
                    "status": "failed",
                    "error": format!("{e:#}"),
                }));
            }
        }
    }
    let output = serde_json::json!({ "org": plan.org, "failed": failed, "results": results });
    if failed > 0 {
        println!("{}", serde_json::to_string_pretty(&output)?);
        anyhow::bail!("{failed} of {} tasks failed", plan.tasks.len());
    }
    Ok(output)
}

fn log_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}
