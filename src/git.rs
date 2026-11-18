//! Backs up a repository's git data, and its wiki's, as git bundles.
//!
//! This is the equivalent of `git clone --mirror` followed by
//! `git bundle create --all`, done in-process with `gix` so that the Lambda
//! function needs no `git` binary. Every ref the server advertises is
//! captured: branches, tags, notes, and GitHub's `refs/pull/*`, which keep the
//! commits of every pull request (including ones from deleted forks) alive.
//!
//! Restore with `git clone --mirror <repo>.bundle`.

use {
    crate::{
        Context,
        event::{Task, TaskResult},
        storage::Metadata,
        util,
    },
    anyhow::{Context as _, Result, anyhow, bail},
    gix::{
        protocol::handshake::Ref,
        remote::{
            Direction,
            fetch::{Status, Tags},
        },
    },
    serde_json::json,
    sha2::{Digest as _, Sha256},
    std::{
        io::Write as _,
        path::{Path, PathBuf},
        sync::atomic::AtomicBool,
    },
};

/// User metadata key on a stored bundle recording which refs it contains.
const REFS_DIGEST: &str = "refs-sha256";
/// User metadata key on a stored bundle recording whether the repository uses Git LFS.
const USES_LFS: &str = "uses-git-lfs";

pub async fn backup(ctx: &Context, repo: &str) -> Result<TaskResult> {
    let mut result = TaskResult::new(Task::Git { repo: repo.to_string() });
    let org = &ctx.config.org;
    let base = format!("repos/{repo}");
    let web = ctx.github.web_url();

    let code = mirror_to_storage(ctx, &format!("{web}/{org}/{repo}.git"), &format!("{base}/git"), repo)
        .await
        .with_context(|| format!("backing up git data of {org}/{repo}"))?;
    result.set("git", code.state);
    result.set("refs", code.refs);
    result.set("bundle_bytes", code.bytes);

    // Always probe: `has_wiki` only says the feature is enabled, and the wiki's
    // git repository does not exist until someone writes a page.
    let wiki_url = format!("{web}/{org}/{repo}.wiki.git");
    let wiki = if ctx.github.git_repository_exists(&wiki_url).await? {
        let wiki = mirror_to_storage(ctx, &wiki_url, &format!("{base}/wiki"), &format!("{repo}.wiki"))
            .await
            .with_context(|| format!("backing up wiki of {org}/{repo}"))?;
        result.set("wiki", wiki.state);
        result.set("wiki_refs", wiki.refs);
        result.set("wiki_bundle_bytes", wiki.bytes);
        Some(wiki)
    } else {
        result.set("wiki", "none");
        None
    };

    let manifest = json!({
        "repository": format!("{org}/{repo}"),
        "backed_up_at": util::now_rfc3339(),
        "tool_version": env!("CARGO_PKG_VERSION"),
        "git": code.describe(),
        "wiki": wiki.as_ref().map(Mirror::describe),
    });
    ctx.storage.put_json(&format!("{base}/git/manifest.json"), &manifest).await?;

    // Checked last, so that everything that can be backed up has been. Git LFS
    // objects live outside the git object store and are not in the bundle; a
    // run that leaves them behind must not look like a success.
    if code.uses_lfs {
        bail!(
            "{org}/{repo} uses Git LFS. Its git history has been backed up, but LFS objects are not supported \
             and were NOT backed up."
        );
    }
    Ok(result)
}

struct Mirror {
    /// `uploaded`, `unchanged`, or `empty`.
    state: &'static str,
    refs: usize,
    bytes: u64,
    digest: String,
    head: Option<String>,
    uses_lfs: bool,
}

impl Mirror {
    fn describe(&self) -> serde_json::Value {
        json!({
            "state": self.state,
            "refs": self.refs,
            "bundle_bytes": self.bytes,
            "refs_sha256": self.digest,
            "head": self.head,
        })
    }
}

/// Mirrors the repository at `url` into `{dir}/{name}.bundle`, alongside a
/// `{dir}/refs.json` listing what the bundle contains.
///
/// If the stored bundle was made from exactly the refs the remote has now, the
/// fetch is skipped entirely. Refs name content-addressed objects, so equal
/// refs mean equal contents, and the expensive download and upload happen only
/// when something was pushed.
async fn mirror_to_storage(ctx: &Context, url: &str, dir: &str, name: &str) -> Result<Mirror> {
    let bundle_key = format!("{dir}/{name}.bundle");
    let stored = ctx.storage.head(&bundle_key).await?;
    let stored_digest = stored.as_ref().and_then(|object| object.metadata.get(REFS_DIGEST).cloned());

    let scratch = tempfile::Builder::new().prefix("git-").tempdir_in(&ctx.config.work_dir)?;
    let fetched = {
        let (url, token, scratch) = (url.to_string(), ctx.github.token().to_string(), scratch.path().to_path_buf());
        tokio::task::spawn_blocking(move || fetch_mirror(&url, &token, &scratch, stored_digest.as_deref()))
            .await
            .context("git fetch panicked")??
    };

    let refs = &fetched.refs;
    let digest = refs.digest();
    let (state, bytes, uses_lfs) = match &fetched.bundle {
        Fetched::Unchanged => {
            tracing::info!(url, refs = refs.refs.len(), "refs unchanged since last backup; skipping fetch");
            // Remembered from when the bundle was made, so that an LFS
            // repository keeps being reported on every run, not just the first.
            let uses_lfs =
                stored.as_ref().is_some_and(|object| object.metadata.get(USES_LFS).is_some_and(|v| v == "true"));
            ("unchanged", stored.map(|object| object.size).unwrap_or(0), uses_lfs)
        }
        Fetched::Empty => {
            tracing::info!(url, "repository has no refs; nothing to bundle");
            ("empty", 0, false)
        }
        Fetched::Bundle { path, uses_lfs } => {
            let bytes = tokio::fs::metadata(path).await?.len();
            tracing::info!(url, refs = refs.refs.len(), bytes, key = bundle_key, "uploading bundle");
            let metadata = Metadata::from([
                (REFS_DIGEST.to_string(), digest.clone()),
                ("ref-count".to_string(), refs.refs.len().to_string()),
                (USES_LFS.to_string(), uses_lfs.to_string()),
            ]);
            ctx.storage.put_file(&bundle_key, path, "application/x-git-bundle", metadata).await?;
            ("uploaded", bytes, *uses_lfs)
        }
    };

    // Written every time (it is a no-op when unchanged) so that a run that died
    // between the two uploads is repaired by the next one.
    let listing = json!({
        "url": url,
        "head": refs.head,
        "refs_sha256": digest,
        "refs": refs.refs.iter().map(|(name, oid)| json!({ "name": name, "oid": oid })).collect::<Vec<_>>(),
    });
    ctx.storage.put_json(&format!("{dir}/refs.json"), &listing).await?;

    Ok(Mirror { state, refs: refs.refs.len(), bytes, digest, head: refs.head.clone(), uses_lfs })
}

/// The refs a remote advertises.
struct RemoteRefs {
    /// The branch `HEAD` points at, e.g. `refs/heads/main`.
    head: Option<String>,
    /// `(name, object id)`, sorted by name. Includes `HEAD` when it resolves.
    /// Annotated tags are listed by the id of the tag object, as in a bundle.
    refs: Vec<(String, String)>,
}

impl RemoteRefs {
    fn from_advertisement(advertised: &[Ref]) -> Result<Self> {
        let mut head = None;
        let mut refs = Vec::with_capacity(advertised.len());
        for advertised_ref in advertised {
            let (name, oid) = match advertised_ref {
                Ref::Direct { full_ref_name, object } => (full_ref_name, Some(object)),
                Ref::Peeled { full_ref_name, tag, .. } => (full_ref_name, Some(tag)),
                Ref::Symbolic { full_ref_name, target, tag, object } => {
                    if full_ref_name == "HEAD" {
                        head = Some(target.to_string());
                    }
                    (full_ref_name, Some(tag.as_ref().unwrap_or(object)))
                }
                Ref::Unborn { full_ref_name, target } => {
                    if full_ref_name == "HEAD" {
                        head = Some(target.to_string());
                    }
                    (full_ref_name, None)
                }
            };
            let name = name.to_string();
            // Ref names go into a line-oriented file header.
            if name.is_empty() || name.contains(['\n', '\r', '\0', ' ']) {
                bail!("remote advertised a ref with an invalid name: {name:?}");
            }
            if let Some(oid) = oid {
                refs.push((name, oid.to_string()));
            }
        }
        refs.sort();
        refs.dedup();
        Ok(Self { head, refs })
    }

    /// A fingerprint of the complete ref state. The leading tag versions the
    /// bundle format: changing it forces every bundle to be rebuilt.
    fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"git-bundle-v2/all-refs/1\n");
        hasher.update(format!("HEAD -> {}\n", self.head.as_deref().unwrap_or("")));
        for (name, oid) in &self.refs {
            hasher.update(format!("{oid} {name}\n"));
        }
        util::hex(&hasher.finalize())
    }

    /// The header of a version 2 git bundle with no prerequisites: the format
    /// line, one `<oid> <refname>` line per ref, and a blank line. The pack
    /// follows immediately.
    fn bundle_header(&self) -> Vec<u8> {
        let mut header = b"# v2 git bundle\n".to_vec();
        for (name, oid) in &self.refs {
            header.extend_from_slice(format!("{oid} {name}\n").as_bytes());
        }
        header.push(b'\n');
        header
    }
}

enum Fetched {
    /// The remote's refs match `known_digest`; nothing was downloaded.
    Unchanged,
    /// The remote has no refs at all.
    Empty,
    Bundle {
        path: PathBuf,
        uses_lfs: bool,
    },
}

struct FetchOutcome {
    refs: RemoteRefs,
    bundle: Fetched,
}

/// Fetches every ref of `url` into a fresh bare repository under `scratch` and
/// writes a bundle of the result. Blocking; run on a blocking thread.
fn fetch_mirror(url: &str, token: &str, scratch: &Path, known_digest: Option<&str>) -> Result<FetchOutcome> {
    let repo = gix::init_bare(scratch.join("mirror.git")).map_err(describe)?;
    let remote = repo
        .remote_at(url)
        .map_err(describe)?
        // Tags are covered by the refspec; no implicit tag-following needed.
        .with_fetch_tags(Tags::None)
        .with_refspecs(["+refs/*:refs/*"], Direction::Fetch)
        .map_err(describe)?;

    let token = token.to_string();
    let connection = remote.connect(Direction::Fetch).map_err(describe)?.with_credentials(move |action| {
        // Never consult credential helpers or prompt: there is exactly one
        // credential, and it is only ever asked for by the host being cloned.
        Ok(match action {
            gix::credentials::helper::Action::Get(context) => Some(gix::credentials::protocol::Outcome {
                identity: gix::sec::identity::Account {
                    username: "x-access-token".into(),
                    password: token.clone(),
                    oauth_refresh_token: None,
                },
                next: context.into(),
            }),
            _ => None,
        })
    });

    // Ask for the complete advertisement (not just refs/*) so that HEAD and
    // the branch it points to are included.
    let options = gix::remote::ref_map::Options { prefix_from_spec_as_filter_on_remote: false, ..Default::default() };
    let prepare = connection.prepare_fetch(gix::progress::Discard, options).map_err(describe)?;

    let refs = RemoteRefs::from_advertisement(&prepare.ref_map().remote_refs)?;
    if refs.refs.is_empty() {
        return Ok(FetchOutcome { refs, bundle: Fetched::Empty });
    }
    if known_digest == Some(refs.digest().as_str()) {
        return Ok(FetchOutcome { refs, bundle: Fetched::Unchanged });
    }

    let never_interrupt = AtomicBool::new(false);
    let outcome = prepare
        // Loose refs would collide on case-insensitive filesystems.
        .with_write_packed_refs_only(true)
        .receive(gix::progress::Discard, &never_interrupt)
        .map_err(describe)?;

    let pack = match outcome.status {
        Status::Change { write_pack_bundle, .. } => {
            write_pack_bundle.data_path.context("fetch completed without writing a pack file")?
        }
        Status::NoPackReceived { .. } => bail!("remote advertised {} refs but sent no objects", refs.refs.len()),
    };

    // A fetch into an empty repository yields exactly one self-contained pack
    // holding everything reachable from the fetched refs, which is precisely
    // the payload of a bundle with no prerequisites.
    let bundle = scratch.join("mirror.bundle");
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&bundle)?);
    out.write_all(&refs.bundle_header())?;
    std::io::copy(&mut std::fs::File::open(&pack)?, &mut out).context("copying pack into bundle")?;
    out.into_inner().map_err(|e| anyhow!("flushing bundle: {e}"))?.sync_all()?;

    let uses_lfs = uses_lfs(&repo, &refs).unwrap_or_else(|e| {
        tracing::warn!(url, error = format!("{e:#}"), "could not check for Git LFS usage");
        false
    });
    Ok(FetchOutcome { refs, bundle: Fetched::Bundle { path: bundle, uses_lfs } })
}

/// Whether the default branch routes any path through Git LFS, judged by the
/// top-level `.gitattributes`.
fn uses_lfs(repo: &gix::Repository, refs: &RemoteRefs) -> Result<bool> {
    let Some((_, head)) = refs.refs.iter().find(|(name, _)| name == "HEAD") else {
        return Ok(false);
    };
    let head = gix::ObjectId::from_hex(head.as_bytes())?;
    let tree = repo.find_object(head).map_err(describe)?.peel_to_tree().map_err(describe)?;
    let Some(entry) = tree.lookup_entry_by_path(".gitattributes").map_err(describe)? else {
        return Ok(false);
    };
    let attributes = entry.object().map_err(describe)?;
    Ok(attributes.data.windows(10).any(|window| window == b"filter=lfs"))
}

/// Flattens a `gix` error and its causes (which its `Debug` output lists, one
/// per line) into a single-line `anyhow` error.
fn describe(error: impl std::fmt::Debug) -> anyhow::Error {
    anyhow!("{}", format!("{error:?}").split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(head: Option<&str>, list: &[(&str, &str)]) -> RemoteRefs {
        RemoteRefs {
            head: head.map(str::to_string),
            refs: list.iter().map(|(name, oid)| (name.to_string(), oid.to_string())).collect(),
        }
    }

    const A: &str = "0c9068413dc43154f2c9350f6307a2ff43bd5613";
    const B: &str = "7b57fd0b1aad0f0a9868f40cb2c929355ccd1db0";

    #[test]
    fn bundle_header_matches_the_git_format() {
        let r = refs(Some("refs/heads/master"), &[("HEAD", A), ("refs/heads/master", A), ("refs/pull/1/head", B)]);
        assert_eq!(
            String::from_utf8(r.bundle_header()).unwrap(),
            format!("# v2 git bundle\n{A} HEAD\n{A} refs/heads/master\n{B} refs/pull/1/head\n\n")
        );
    }

    #[test]
    fn digest_changes_with_any_ref_change() {
        let base = refs(Some("refs/heads/master"), &[("HEAD", A), ("refs/heads/master", A)]);
        assert_eq!(base.digest(), refs(Some("refs/heads/master"), &[("HEAD", A), ("refs/heads/master", A)]).digest());

        let moved = refs(Some("refs/heads/master"), &[("HEAD", B), ("refs/heads/master", B)]);
        let added = refs(Some("refs/heads/master"), &[("HEAD", A), ("refs/heads/master", A), ("refs/tags/v1", A)]);
        let rehead = refs(Some("refs/heads/main"), &[("HEAD", A), ("refs/heads/master", A)]);
        for other in [&moved, &added, &rehead] {
            assert_ne!(base.digest(), other.digest());
        }
    }
}
