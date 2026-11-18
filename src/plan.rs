//! Works out what a full backup consists of right now.

use {
    crate::{
        Context,
        event::{Plan, Request, Task},
    },
    anyhow::{Context as _, Result},
};

/// Lists the organization's repositories and returns one invocation payload
/// per task: one for the organization itself, then two per repository.
///
/// Every repository the token can see is included: public and private, forks,
/// archived repositories, templates. Repositories are discovered afresh on
/// each run, so new ones are picked up without any configuration change.
pub async fn plan(ctx: &Context) -> Result<Plan> {
    let org = &ctx.config.org;
    let repositories = ctx
        .github
        .paginate(&format!("orgs/{org}/repos?type=all&sort=full_name"))
        .await
        .with_context(|| format!("listing repositories of {org}"))?;

    let mut names: Vec<String> = repositories
        .iter()
        .map(|repository| repository["name"].as_str().map(str::to_string).context("repository without a name"))
        .collect::<Result<_>>()?;
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();

    let mut tasks = vec![Request::Backup(Task::Org)];
    for name in &names {
        for task in [Task::Git { repo: name.clone() }, Task::Metadata { repo: name.clone() }] {
            // A name this program would refuse later is better refused now,
            // where it fails the whole run visibly.
            task.validate()?;
            tasks.push(Request::Backup(task));
        }
    }
    tracing::info!(org, repositories = names.len(), tasks = tasks.len(), "planned backup");
    Ok(Plan { org: org.clone(), repositories: names.len(), tasks })
}
