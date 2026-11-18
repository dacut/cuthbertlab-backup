//! The JSON contract between the Step Functions state machine and the Lambda
//! function.

use std::fmt;

use {
    anyhow::{Result, bail},
    serde::{Deserialize, Serialize},
    serde_json::{Map, Value},
};

/// A Lambda invocation payload.
///
/// ```json
/// {"action": "plan"}
/// {"action": "backup", "part": "org"}
/// {"action": "backup", "part": "git", "repo": "music21"}
/// {"action": "backup", "part": "metadata", "repo": "music21"}
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    /// List the organization's repositories and return one task per unit of work.
    Plan,
    /// Run a single backup task.
    Backup(Task),
}

/// One independently runnable (and retryable) unit of backup work.
///
/// A repository is split into two tasks so that the large-but-simple git
/// mirror and the many-small-requests API crawl each get a full Lambda timeout
/// and can be retried separately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "part", rename_all = "snake_case")]
pub enum Task {
    /// Organization-level data: profile, members, teams, projects, repository index.
    Org,
    /// A repository's git data: all refs as a bundle, plus its wiki.
    Git { repo: String },
    /// Everything GitHub stores about a repository outside of git.
    Metadata { repo: String },
}

impl Task {
    /// Rejects repository names that could not have come from GitHub. Names end
    /// up in S3 keys and URLs, so this is checked before anything else happens.
    pub fn validate(&self) -> Result<()> {
        match self {
            Task::Org => Ok(()),
            Task::Git { repo } | Task::Metadata { repo } => {
                let ok = !repo.is_empty()
                    && repo.len() <= 100
                    && repo != "."
                    && repo != ".."
                    && repo.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
                if !ok {
                    bail!("invalid repository name {repo:?}");
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Task::Org => write!(f, "org"),
            Task::Git { repo } => write!(f, "git:{repo}"),
            Task::Metadata { repo } => write!(f, "metadata:{repo}"),
        }
    }
}

/// The response to [`Request::Plan`].
#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub org: String,
    pub repositories: usize,
    /// Ready-made invocation payloads, one per task.
    pub tasks: Vec<Request>,
}

/// The response to [`Request::Backup`]. Kept small: Step Functions collects one
/// of these per task and caps the combined state at 256 KiB.
#[derive(Debug, Serialize, Deserialize)]
pub struct TaskResult {
    pub task: Task,
    pub status: String,
    /// Headline numbers, e.g. `{"issues": 835, "release_assets_uploaded": 2}`.
    pub summary: Map<String, Value>,
    /// Things that were not backed up but do not make the run a failure, such
    /// as endpoints the token is not permitted to read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub elapsed_seconds: f64,
    #[serde(default)]
    pub github_requests: u64,
}

impl TaskResult {
    pub fn new(task: Task) -> Self {
        Self {
            task,
            status: "ok".into(),
            summary: Map::new(),
            warnings: Vec::new(),
            elapsed_seconds: 0.0,
            github_requests: 0,
        }
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.summary.insert(key.to_string(), value.into());
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!(task = %self.task, "{message}");
        // Bounded so that a pathological repository cannot blow the Step
        // Functions payload limit; the full list is in the logs.
        if self.warnings.len() < 50 {
            self.warnings.push(crate::util::truncate(&message, 300));
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    #[test]
    fn requests_round_trip() {
        let cases = [
            (json!({"action": "plan"}), Request::Plan),
            (json!({"action": "backup", "part": "org"}), Request::Backup(Task::Org)),
            (
                json!({"action": "backup", "part": "git", "repo": "music21"}),
                Request::Backup(Task::Git { repo: "music21".into() }),
            ),
            (
                json!({"action": "backup", "part": "metadata", "repo": "music21j"}),
                Request::Backup(Task::Metadata { repo: "music21j".into() }),
            ),
        ];
        for (wire, request) in cases {
            assert_eq!(serde_json::from_value::<Request>(wire.clone()).unwrap(), request);
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        }
    }

    #[test]
    fn unknown_requests_are_rejected() {
        assert!(serde_json::from_value::<Request>(json!({"action": "restore"})).is_err());
        assert!(serde_json::from_value::<Request>(json!({"action": "backup", "part": "git"})).is_err());
        assert!(serde_json::from_value::<Request>(json!({})).is_err());
    }

    #[test]
    fn repository_names_are_validated() {
        for good in ["music21", "midi-js-soundfonts", "music21-mnx", ".github", "a_b.c"] {
            assert!(Task::Git { repo: good.into() }.validate().is_ok(), "{good}");
        }
        for bad in ["", ".", "..", "a/b", "../x", "a b", "a\nb", "caf\u{e9}"] {
            assert!(Task::Metadata { repo: bad.into() }.validate().is_err(), "{bad:?}");
        }
    }
}
