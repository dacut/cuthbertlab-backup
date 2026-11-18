//! Backs up organization-level data: the profile, the list of repositories,
//! people and teams, and Projects (v2), all under `org/`.

use {
    crate::{
        Context,
        event::{Task, TaskResult},
        github::{GraphQlError, unavailable_status},
        graphql, util,
    },
    anyhow::{Context as _, Result, bail},
    serde_json::{Value, json},
};

/// `(name, path under /orgs/{org}/, optional)`. Optional lists need
/// organization membership or ownership to read.
const COLLECTIONS: &[(&str, &str, bool)] = &[
    // The authoritative index of what existed at the time of the backup,
    // including repository settings.
    ("repos", "repos?type=all&sort=full_name", false),
    ("public_members", "public_members", false),
    ("members", "members", true),
    ("outside_collaborators", "outside_collaborators", true),
    ("teams", "teams", true),
    ("rulesets", "rulesets", true),
    ("webhooks", "hooks", true),
    ("actions_variables", "actions/variables", true),
    ("actions_secret_names", "actions/secrets", true),
];

pub async fn backup(ctx: &Context) -> Result<TaskResult> {
    let mut result = TaskResult::new(Task::Org);
    let org = &ctx.config.org;
    let api = format!("orgs/{org}");

    let profile = ctx.github.get_json(&api).await.with_context(|| format!("fetching organization {org}"))?;
    ctx.storage.put_json("org/org.json", &profile).await?;

    let mut collections = serde_json::Map::new();
    let mut skipped = serde_json::Map::new();
    let mut refused = Vec::new();

    for &(name, path, optional) in COLLECTIONS {
        match ctx.github.paginate(&format!("{api}/{path}")).await {
            Ok(mut list) => {
                if name == "teams" {
                    add_team_members(ctx, &api, &mut list).await?;
                }
                let changed = ctx.storage.put_collection(&format!("org/{name}.json.gz"), &list).await?;
                collections.insert(name.into(), json!({ "count": list.len(), "changed": changed }));
                result.set(name, list.len());
            }
            Err(e) => match unavailable_status(&e) {
                Some(status) if optional => {
                    refused.push(name);
                    skipped.insert(name.into(), json!(format!("HTTP {status}: not readable with this token")));
                }
                _ => return Err(e.context(format!("fetching organization {name}"))),
            },
        }
    }
    if !refused.is_empty() {
        result.warn(format!(
            "not backed up, because the token is not allowed to read them (most need organization owner rights): {}",
            refused.join(", ")
        ));
    }

    if ctx.config.backup_projects {
        match graphql::org_projects(&ctx.github, org).await {
            Ok(projects) => {
                let changed = ctx.storage.put_collection("org/projects.json.gz", &projects).await?;
                collections.insert("projects".into(), json!({ "count": projects.len(), "changed": changed }));
                result.set("projects", projects.len());
            }
            // Deliberately fatal. Projects were asked for; a token that cannot
            // read them means an incomplete backup, and that should be noticed
            // now rather than on the day a project needs restoring.
            Err(e) if e.downcast_ref::<GraphQlError>().is_some_and(GraphQlError::is_permission_problem) => {
                // Everything else is already stored; record that before failing.
                skipped.insert("projects".into(), json!("the token lacks permission to read projects"));
                write_manifest(ctx, &collections, &skipped, &result.warnings).await?;
                bail!(
                    "the GitHub token cannot read {org}'s Projects. Grant it the read:project scope (classic \
                     token) or the organization permission \"Projects: read-only\" (fine-grained token), or set \
                     BACKUP_PROJECTS=false to leave projects out of the backup. GitHub said: {e}"
                );
            }
            Err(e) => return Err(e.context("fetching organization projects")),
        }
    } else {
        skipped.insert("projects".into(), json!("disabled by BACKUP_PROJECTS=false"));
    }

    write_manifest(ctx, &collections, &skipped, &result.warnings).await?;
    Ok(result)
}

/// Adds each team's roster and repository grants to its entry, as
/// `members` and `repositories`.
async fn add_team_members(ctx: &Context, api: &str, teams: &mut [Value]) -> Result<()> {
    for team in teams {
        let Some(slug) = team["slug"].as_str().map(str::to_string) else { continue };
        for (field, path) in [("members", "members"), ("repositories", "repos")] {
            match ctx.github.paginate(&format!("{api}/teams/{slug}/{path}")).await {
                Ok(list) => team[field] = Value::Array(list),
                Err(e) if unavailable_status(&e).is_some() => {}
                Err(e) => return Err(e.context(format!("fetching {field} of team {slug}"))),
            }
        }
    }
    Ok(())
}

async fn write_manifest(
    ctx: &Context,
    collections: &serde_json::Map<String, Value>,
    skipped: &serde_json::Map<String, Value>,
    warnings: &[String],
) -> Result<()> {
    let manifest = json!({
        "organization": ctx.config.org,
        "backed_up_at": util::now_rfc3339(),
        "tool_version": env!("CARGO_PKG_VERSION"),
        "collections": collections,
        "skipped": skipped,
        "warnings": warnings,
    });
    ctx.storage.put_json("org/manifest.json", &manifest).await?;
    Ok(())
}
