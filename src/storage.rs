//! Where backups are written: S3 in production, a local directory for testing.
//!
//! Nothing here ever deletes an object. A repository that disappears from
//! GitHub keeps its last backup, and (with bucket versioning) an object that is
//! overwritten keeps its previous versions.

use {
    crate::{config::Config, util},
    anyhow::{Context as _, Result, bail},
    aws_sdk_s3::{
        operation::upload_part::UploadPartOutput,
        primitives::{ByteStream, Length},
        types::{CompletedMultipartUpload, CompletedPart},
    },
    futures::{StreamExt as _, TryStreamExt as _},
    serde::Serialize,
    serde_json::Value,
    std::{
        collections::{BTreeMap, HashMap},
        path::{Path, PathBuf},
    },
};

/// Files larger than this are uploaded in parts of this size. S3 allows 10,000
/// parts, so this caps a single object at 640 GiB.
const PART_SIZE: u64 = 64 * 1024 * 1024;
const PART_CONCURRENCY: usize = 4;

/// User metadata key holding the SHA-256 of an object's uncompressed content.
const SHA256: &str = "sha256";

pub type Metadata = BTreeMap<String, String>;

pub struct ObjectInfo {
    pub size: u64,
    pub metadata: HashMap<String, String>,
}

pub enum Storage {
    S3 { client: aws_sdk_s3::Client, bucket: String, prefix: String },
    Local { root: PathBuf },
}

impl Storage {
    pub fn new(config: &Config, aws: &aws_config::SdkConfig) -> Result<Self> {
        Ok(match &config.local_dir {
            Some(root) => {
                std::fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
                Storage::Local { root: root.canonicalize()? }
            }
            None => Storage::S3 {
                // Path-style addressing is for S3-compatible stores and local
                // emulators; AWS itself never needs it.
                client: aws_sdk_s3::Client::from_conf(
                    aws_sdk_s3::config::Builder::from(aws).force_path_style(config.s3_force_path_style).build(),
                ),
                bucket: config.bucket.clone(),
                prefix: config.prefix.clone(),
            },
        })
    }

    /// Human-readable location of `key`, for log messages.
    pub fn describe(&self, key: &str) -> String {
        match self {
            Storage::S3 { bucket, prefix, .. } => format!("s3://{bucket}/{prefix}{key}"),
            Storage::Local { root } => root.join(key).display().to_string(),
        }
    }

    /// Returns the size and user metadata of `key`, or `None` if it does not exist.
    pub async fn head(&self, key: &str) -> Result<Option<ObjectInfo>> {
        check_key(key)?;
        match self {
            Storage::S3 { client, bucket, prefix } => {
                match client.head_object().bucket(bucket).key(format!("{prefix}{key}")).send().await {
                    Ok(head) => Ok(Some(ObjectInfo {
                        size: head.content_length().unwrap_or(0).max(0) as u64,
                        metadata: head.metadata().cloned().unwrap_or_default(),
                    })),
                    Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(None),
                    Err(e) => Err(util::aws_error(e)).with_context(|| format!("cannot read {}", self.describe(key))),
                }
            }
            Storage::Local { root } => {
                let size = match tokio::fs::metadata(root.join(key)).await {
                    Ok(meta) => meta.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let metadata = match tokio::fs::read(sidecar(root, key)).await {
                    Ok(bytes) => serde_json::from_slice(&bytes)?,
                    Err(_) => HashMap::new(),
                };
                Ok(Some(ObjectInfo { size, metadata }))
            }
        }
    }

    /// Lists every key under `prefix`, with sizes.
    pub async fn list(&self, prefix: &str) -> Result<HashMap<String, u64>> {
        check_key(prefix)?;
        let mut found = HashMap::new();
        match self {
            Storage::S3 { client, bucket, prefix: base } => {
                let mut pages =
                    client.list_objects_v2().bucket(bucket).prefix(format!("{base}{prefix}")).into_paginator().send();
                while let Some(page) = pages.next().await {
                    let page = page
                        .map_err(util::aws_error)
                        .with_context(|| format!("cannot list {}", self.describe(prefix)))?;
                    for object in page.contents() {
                        if let Some(key) = object.key().and_then(|k| k.strip_prefix(base.as_str())) {
                            found.insert(key.to_string(), object.size().unwrap_or(0).max(0) as u64);
                        }
                    }
                }
            }
            Storage::Local { root } => {
                let (root, prefix) = (root.clone(), prefix.to_string());
                found = tokio::task::spawn_blocking(move || list_local(&root, &prefix)).await??;
            }
        }
        Ok(found)
    }

    /// Stores `body` at `key`, replacing whatever is there.
    pub async fn put_bytes(&self, key: &str, body: Vec<u8>, content_type: &str, metadata: Metadata) -> Result<()> {
        check_key(key)?;
        match self {
            Storage::S3 { client, bucket, prefix } => {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(format!("{prefix}{key}"))
                    .content_type(content_type)
                    .set_metadata(Some(metadata.into_iter().collect()))
                    .body(ByteStream::from(body))
                    .send()
                    .await
                    .map_err(util::aws_error)
                    .with_context(|| format!("cannot write {}", self.describe(key)))?;
            }
            Storage::Local { root } => {
                let path = root.join(key);
                tokio::fs::create_dir_all(path.parent().context("key has no parent")?).await?;
                tokio::fs::write(&path, body).await?;
                write_sidecar(root, key, &metadata).await?;
            }
        }
        Ok(())
    }

    /// Stores the file at `path` at `key`, replacing whatever is there. Large
    /// files are sent as a multipart upload.
    pub async fn put_file(&self, key: &str, path: &Path, content_type: &str, metadata: Metadata) -> Result<()> {
        check_key(key)?;
        let size = tokio::fs::metadata(path).await?.len();
        match self {
            Storage::S3 { client, bucket, prefix } => {
                let full_key = format!("{prefix}{key}");
                let result = if size <= PART_SIZE {
                    async {
                        client
                            .put_object()
                            .bucket(bucket)
                            .key(&full_key)
                            .content_type(content_type)
                            .set_metadata(Some(metadata.into_iter().collect()))
                            .body(ByteStream::from_path(path).await?)
                            .send()
                            .await
                            .map_err(util::aws_error)?;
                        Ok(())
                    }
                    .await
                } else {
                    multipart_upload(client, bucket, &full_key, path, size, content_type, metadata).await
                };
                result.with_context(|| format!("cannot write {}", self.describe(key)))?;
            }
            Storage::Local { root } => {
                let dest = root.join(key);
                tokio::fs::create_dir_all(dest.parent().context("key has no parent")?).await?;
                tokio::fs::copy(path, &dest).await?;
                write_sidecar(root, key, &metadata).await?;
            }
        }
        Ok(())
    }

    /// Stores a single JSON document, pretty-printed, unless the stored copy is
    /// already identical. Returns whether anything was written.
    pub async fn put_json(&self, key: &str, value: &impl Serialize) -> Result<bool> {
        let mut body = serde_json::to_vec_pretty(value)?;
        body.push(b'\n');
        self.put_if_changed(key, body).await
    }

    /// Stores a list of JSON documents as a gzipped JSON array with one element
    /// per line, unless the stored copy is already identical. Returns whether
    /// anything was written.
    ///
    /// One element per line keeps the file valid JSON (`gunzip -c | jq .`)
    /// while also making it greppable and line-diffable.
    pub async fn put_collection(&self, key: &str, items: &[Value]) -> Result<bool> {
        if !key.ends_with(".json.gz") {
            bail!("collection key {key:?} must end in .json.gz");
        }
        let mut body = Vec::new();
        body.extend_from_slice(b"[");
        for (i, item) in items.iter().enumerate() {
            body.extend_from_slice(if i == 0 { b"\n" } else { b",\n" });
            serde_json::to_writer(&mut body, item)?;
        }
        body.extend_from_slice(b"\n]\n");
        self.put_if_changed(key, body).await
    }

    /// Writes `body` (gzipping it if the key ends in `.gz`) unless the object
    /// already stored has the same content hash.
    ///
    /// Skipping unchanged content matters with bucket versioning: most of the
    /// organization's repositories are dormant, and rewriting their files on
    /// every run would only pile up identical noncurrent versions.
    async fn put_if_changed(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let digest = util::sha256_hex(&body);
        if let Some(existing) = self.head(key).await?
            && existing.metadata.get(SHA256) == Some(&digest)
        {
            return Ok(false);
        }
        let metadata = Metadata::from([(SHA256.to_string(), digest)]);
        if key.ends_with(".gz") {
            self.put_bytes(key, util::gzip(&body)?, "application/gzip", metadata).await?;
        } else {
            self.put_bytes(key, body, "application/json", metadata).await?;
        }
        Ok(true)
    }
}

async fn multipart_upload(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    path: &Path,
    size: u64,
    content_type: &str,
    metadata: Metadata,
) -> Result<()> {
    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .content_type(content_type)
        .set_metadata(Some(metadata.into_iter().collect()))
        .send()
        .await
        .map_err(util::aws_error)?
        .upload_id()
        .context("S3 did not return an upload ID")?
        .to_string();

    let part_count = size.div_ceil(PART_SIZE);
    let upload_id_ref = upload_id.as_str();
    let parts: Result<Vec<CompletedPart>> = futures::stream::iter(0..part_count)
        .map(|index| async move {
            let offset = index * PART_SIZE;
            let length = PART_SIZE.min(size - offset);
            let part_number = (index + 1) as i32;
            let body = ByteStream::read_from().path(path).offset(offset).length(Length::Exact(length)).build().await?;
            let part = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id_ref)
                .part_number(part_number)
                .body(body)
                .send()
                .await
                .map_err(util::aws_error)
                .with_context(|| format!("part {part_number}"))?;
            Ok(completed_part(part_number, &part))
        })
        .buffer_unordered(PART_CONCURRENCY)
        .try_collect()
        .await;

    let complete = async {
        let mut parts = parts?;
        parts.sort_by_key(|part| part.part_number());
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .send()
            .await
            .map_err(util::aws_error)?;
        Ok(())
    }
    .await;

    if complete.is_err() {
        // Best effort: the bucket's lifecycle rule also reaps abandoned uploads.
        let _ = client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
    }
    complete
}

/// The entry for an uploaded part in the request that completes the upload.
///
/// The ETag alone identifies the part. The SDK checksums each part in transit
/// and S3 echoes that checksum back, but it must not be repeated here: the
/// upload was not created with a checksum algorithm, and S3 then rejects the
/// completion with `InvalidPart`.
fn completed_part(part_number: i32, uploaded: &UploadPartOutput) -> CompletedPart {
    CompletedPart::builder().part_number(part_number).set_e_tag(uploaded.e_tag().map(str::to_string)).build()
}

/// Keys are built by this program from validated or encoded parts; this is a
/// last line of defence against writing outside the intended location.
fn check_key(key: &str) -> Result<()> {
    if key.starts_with('/') || key.contains('\0') || key.split('/').any(|part| part == "." || part == "..") {
        bail!("refusing to use unsafe storage key {key:?}");
    }
    Ok(())
}

/// Local stand-in for S3 user metadata.
fn sidecar(root: &Path, key: &str) -> PathBuf {
    root.join(".metadata").join(format!("{key}.json"))
}

async fn write_sidecar(root: &Path, key: &str, metadata: &Metadata) -> Result<()> {
    let path = sidecar(root, key);
    tokio::fs::create_dir_all(path.parent().context("key has no parent")?).await?;
    tokio::fs::write(path, serde_json::to_vec(metadata)?).await?;
    Ok(())
}

fn list_local(root: &Path, prefix: &str) -> Result<HashMap<String, u64>> {
    fn walk(root: &Path, dir: &Path, prefix: &str, found: &mut HashMap<String, u64>) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if path != root.join(".metadata") {
                    walk(root, &path, prefix, found)?;
                }
            } else if let Some(key) = path.strip_prefix(root).ok().and_then(Path::to_str)
                && key.starts_with(prefix)
            {
                found.insert(key.to_string(), entry.metadata()?.len());
            }
        }
        Ok(())
    }
    let mut found = HashMap::new();
    walk(root, root, prefix, &mut found)?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    fn local(dir: &Path) -> Storage {
        Storage::Local { root: dir.to_path_buf() }
    }

    #[tokio::test]
    async fn unchanged_content_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let storage = local(dir.path());
        let items = vec![json!({"number": 1, "title": "a"}), json!({"number": 2, "title": "b"})];

        assert!(storage.put_collection("repos/x/metadata/issues.json.gz", &items).await.unwrap());
        assert!(!storage.put_collection("repos/x/metadata/issues.json.gz", &items).await.unwrap());
        assert!(storage.put_collection("repos/x/metadata/issues.json.gz", &items[..1]).await.unwrap());

        assert!(storage.put_json("repos/x/metadata/repo.json", &items[0]).await.unwrap());
        assert!(!storage.put_json("repos/x/metadata/repo.json", &items[0]).await.unwrap());
    }

    #[tokio::test]
    async fn collections_are_gzipped_json_arrays_with_one_item_per_line() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let storage = local(dir.path());
        let items = vec![json!({"z": 1, "a": "line\nbreak"}), json!({"b": [1, 2]})];
        storage.put_collection("c.json.gz", &items).await.unwrap();

        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(dir.path().join("c.json.gz")).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text.lines().count(), 4, "{text}");
        // Field order is preserved exactly as GitHub sent it.
        assert!(text.contains(r#"{"z":1,"a":"line\nbreak"}"#), "{text}");
        assert_eq!(serde_json::from_str::<Vec<Value>>(&text).unwrap(), items);

        storage.put_collection("empty.json.gz", &[]).await.unwrap();
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(dir.path().join("empty.json.gz")).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(serde_json::from_str::<Vec<Value>>(&text).unwrap(), Vec::<Value>::new());
    }

    #[tokio::test]
    async fn head_and_list_reflect_what_was_stored() {
        let dir = tempfile::tempdir().unwrap();
        let storage = local(dir.path());
        assert!(storage.head("a/b.bin").await.unwrap().is_none());

        let metadata = Metadata::from([("refs-sha256".to_string(), "abc".to_string())]);
        storage.put_bytes("a/b.bin", vec![1, 2, 3], "application/octet-stream", metadata).await.unwrap();
        storage.put_bytes("a/c/d.bin", vec![1], "application/octet-stream", Metadata::new()).await.unwrap();
        storage.put_bytes("z.bin", vec![1], "application/octet-stream", Metadata::new()).await.unwrap();

        let info = storage.head("a/b.bin").await.unwrap().unwrap();
        assert_eq!(info.size, 3);
        assert_eq!(info.metadata.get("refs-sha256").map(String::as_str), Some("abc"));

        let listed = storage.list("a/").await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed["a/b.bin"], 3);
        assert_eq!(listed["a/c/d.bin"], 1);
    }

    #[tokio::test]
    async fn unsafe_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let storage = local(dir.path());
        for key in ["../escape", "a/../../escape", "/absolute", "a/./b"] {
            assert!(storage.put_bytes(key, vec![], "x", Metadata::new()).await.is_err(), "{key}");
        }
    }

    #[test]
    fn completed_parts_carry_no_checksum() {
        // What S3 answers to an UploadPart sent with the SDK's default checksum.
        let uploaded = UploadPartOutput::builder()
            .e_tag("\"d974a79b3e3b2c6fdfa443d557dfe383\"")
            .checksum_crc32("EUZqjw==")
            .checksum_crc32_c("x")
            .checksum_crc64_nvme("x")
            .build();
        let part = completed_part(7, &uploaded);
        assert_eq!(part.part_number(), Some(7));
        assert_eq!(part.e_tag(), Some("\"d974a79b3e3b2c6fdfa443d557dfe383\""));
        // S3 rejects the completion with InvalidPart if any of these is sent.
        assert_eq!(part, CompletedPart::builder().part_number(7).set_e_tag(part.e_tag().map(str::to_string)).build());
    }

    /// A [`Storage::S3`] on a new bucket in the local emulator, along with a
    /// client and the bucket name for looking at what really got stored.
    async fn emulated() -> (Storage, aws_sdk_s3::Client, String) {
        let aws = crate::emulator::aws().await;
        let client =
            aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&aws).force_path_style(true).build());
        let bucket = crate::emulator::unique("storage-test");
        client.create_bucket().bucket(&bucket).send().await.unwrap();
        (Storage::S3 { client: client.clone(), bucket: bucket.clone(), prefix: "pre/fix/".into() }, client, bucket)
    }

    /// Incompressible and different in every part, so that a part stored in
    /// the wrong place or twice cannot go unnoticed.
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[tokio::test]
    #[ignore = "needs a local AWS emulator; run `make test-emulator`"]
    async fn emulator_objects_round_trip() {
        let (storage, client, bucket) = emulated().await;
        let item = json!({"number": 1, "title": "a"});
        let items = std::slice::from_ref(&item);

        // A key that was never written is "not there", not an error.
        assert!(storage.head("repos/x/metadata/repo.json").await.unwrap().is_none());
        assert!(storage.list("repos/x/").await.unwrap().is_empty());

        assert!(storage.put_json("repos/x/metadata/repo.json", &item).await.unwrap());
        assert!(!storage.put_json("repos/x/metadata/repo.json", &item).await.unwrap());
        assert!(storage.put_collection("repos/x/metadata/issues.json.gz", items).await.unwrap());
        assert!(!storage.put_collection("repos/x/metadata/issues.json.gz", items).await.unwrap());
        assert!(storage.put_collection("repos/x/metadata/issues.json.gz", &[]).await.unwrap());
        storage.put_bytes("repos/y/z.bin", vec![1, 2, 3], "application/octet-stream", Metadata::new()).await.unwrap();

        let info = storage.head("repos/x/metadata/repo.json").await.unwrap().unwrap();
        assert_eq!(info.metadata.get(SHA256).map(String::len), Some(64));

        // Keys come back without the storage prefix, and only from the directory asked for.
        let listed = storage.list("repos/x/").await.unwrap();
        let mut keys: Vec<&str> = listed.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["repos/x/metadata/issues.json.gz", "repos/x/metadata/repo.json"]);
        assert_eq!(listed["repos/x/metadata/repo.json"], info.size);

        // The prefix is applied in the bucket.
        let object =
            client.get_object().bucket(&bucket).key("pre/fix/repos/x/metadata/repo.json").send().await.unwrap();
        assert_eq!(object.content_type(), Some("application/json"));
        let body = object.body.collect().await.unwrap().into_bytes();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), item);
    }

    #[tokio::test]
    #[ignore = "needs a local AWS emulator; run `make test-emulator`"]
    async fn emulator_large_files_are_uploaded_in_parts() {
        let (storage, client, bucket) = emulated().await;
        let dir = tempfile::tempdir().unwrap();

        // Two full parts and a short one.
        let content = pseudo_random(2 * PART_SIZE as usize + 12_345);
        let path = dir.path().join("large.bin");
        std::fs::write(&path, &content).unwrap();
        let metadata = Metadata::from([("refs-sha256".to_string(), "abc".to_string())]);
        storage.put_file("repos/x/git/x.bundle", &path, "application/x-git-bundle", metadata.clone()).await.unwrap();

        let info = storage.head("repos/x/git/x.bundle").await.unwrap().unwrap();
        assert_eq!(info.size, content.len() as u64);
        assert_eq!(info.metadata.get("refs-sha256").map(String::as_str), Some("abc"));

        let object = client.get_object().bucket(&bucket).key("pre/fix/repos/x/git/x.bundle").send().await.unwrap();
        assert_eq!(object.content_type(), Some("application/x-git-bundle"));
        // A multipart object's ETag ends in its number of parts.
        let etag = object.e_tag().unwrap_or_default().trim_matches('"').to_string();
        assert!(etag.ends_with("-3"), "not a three-part upload: {etag}");
        let stored = object.body.collect().await.unwrap().into_bytes();
        // Not assert_eq: a failure would print both 128 MiB buffers.
        assert!(stored[..] == content[..], "the stored object differs from the file");

        // Nothing is left half-uploaded.
        let pending = client.list_multipart_uploads().bucket(&bucket).send().await.unwrap();
        assert!(pending.uploads().is_empty());

        // At or below the part size, a file is a single ordinary upload.
        let small = dir.path().join("small.bin");
        std::fs::write(&small, &content[..1000]).unwrap();
        storage.put_file("repos/x/releases/v1/small.bin", &small, "application/octet-stream", metadata).await.unwrap();
        let object =
            client.get_object().bucket(&bucket).key("pre/fix/repos/x/releases/v1/small.bin").send().await.unwrap();
        assert!(!object.e_tag().unwrap_or_default().contains('-'));
        assert!(object.body.collect().await.unwrap().into_bytes()[..] == content[..1000]);
    }
}
