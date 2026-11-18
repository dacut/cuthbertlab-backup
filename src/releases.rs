//! Backs up the binaries attached to releases.
//!
//! Release notes and asset listings are part of the `releases` collection; this
//! module stores the files themselves at
//! `repos/<repo>/releases/<tag>/<file name>`. (The automatic "Source code"
//! archives are not stored: they are generated from the tag, which is in the
//! git bundle.)

use {
    crate::{Context, storage::Metadata, util::key_segment},
    anyhow::{Context as _, Result, bail},
    futures::{StreamExt as _, TryStreamExt as _},
    serde_json::Value,
};

const ASSET_ID: &str = "github-asset-id";
const ASSET_UPDATED_AT: &str = "github-updated-at";

/// Simultaneous asset transfers. Each one is buffered on local disk.
const CONCURRENCY: usize = 4;

#[derive(Default)]
pub struct Assets {
    pub total: usize,
    pub uploaded: usize,
    /// Combined size of all assets, uploaded now or earlier.
    pub bytes: u64,
}

struct Asset<'a> {
    id: u64,
    name: &'a str,
    size: u64,
    updated_at: &'a str,
    key: String,
}

pub async fn backup_assets(ctx: &Context, repo: &str, releases: &[Value]) -> Result<Assets> {
    let mut assets = Vec::new();
    for release in releases {
        // Draft releases may not have a tag yet.
        let tag = match release["tag_name"].as_str() {
            Some(tag) if !tag.is_empty() => tag.to_string(),
            _ => format!("untagged-{}", release["id"].as_u64().context("release without an id")?),
        };
        for asset in release["assets"].as_array().map(Vec::as_slice).unwrap_or_default() {
            // Anything other than "uploaded" is an upload that never finished.
            if asset["state"].as_str() != Some("uploaded") {
                continue;
            }
            let name = asset["name"].as_str().context("release asset without a name")?;
            assets.push(Asset {
                id: asset["id"].as_u64().context("release asset without an id")?,
                name,
                size: asset["size"].as_u64().context("release asset without a size")?,
                updated_at: asset["updated_at"].as_str().unwrap_or_default(),
                key: format!("repos/{repo}/releases/{}/{}", key_segment(&tag), key_segment(name)),
            });
        }
    }

    let uploaded: Vec<bool> = futures::stream::iter(&assets)
        .map(|asset| async move {
            backup_asset(ctx, repo, asset).await.with_context(|| format!("release asset {} ({})", asset.name, asset.id))
        })
        .buffer_unordered(CONCURRENCY)
        .try_collect()
        .await?;

    Ok(Assets {
        total: assets.len(),
        uploaded: uploaded.into_iter().filter(|uploaded| *uploaded).count(),
        bytes: assets.iter().map(|asset| asset.size).sum(),
    })
}

/// Stores one asset unless the stored copy is already this exact upload.
/// Returns whether anything was transferred.
async fn backup_asset(ctx: &Context, repo: &str, asset: &Asset<'_>) -> Result<bool> {
    // Replacing an asset on GitHub means deleting and re-uploading it, which
    // gives it a new id, so (id, updated_at, size) pins the content.
    if let Some(stored) = ctx.storage.head(&asset.key).await?
        && stored.size == asset.size
        && stored.metadata.get(ASSET_ID) == Some(&asset.id.to_string())
        && stored.metadata.get(ASSET_UPDATED_AT).map(String::as_str) == Some(asset.updated_at)
    {
        return Ok(false);
    }

    let scratch = tempfile::Builder::new().prefix("asset-").tempfile_in(&ctx.config.work_dir)?;
    let url = format!("{}/repos/{}/{repo}/releases/assets/{}", ctx.config.api_url, ctx.config.org, asset.id);
    // Asking the asset's API URL for octet-stream yields the file itself.
    let downloaded = ctx.github.download(&url, Some("application/octet-stream"), true, scratch.path()).await?;
    if downloaded.size != asset.size {
        bail!("downloaded {} bytes but GitHub lists the asset as {} bytes", downloaded.size, asset.size);
    }

    let metadata = Metadata::from([
        (ASSET_ID.to_string(), asset.id.to_string()),
        (ASSET_UPDATED_AT.to_string(), asset.updated_at.to_string()),
    ]);
    let content_type = downloaded.content_type.as_deref().unwrap_or("application/octet-stream");
    ctx.storage.put_file(&asset.key, scratch.path(), content_type, metadata).await?;
    tracing::info!(key = asset.key, bytes = asset.size, "stored release asset");
    Ok(true)
}
