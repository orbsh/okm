//! Where a lake lives.
//!
//! A lake is a directory tree and a log over it, and none of that cares whether the bytes
//! sit on a local disk or in a bucket: the log says which files exist, the files are
//! immutable once written, and nothing is edited in place. This module is that
//! indifference made explicit — one trait, two answers, and every verb in this crate
//! speaking through it.
//!
//! Keys are relative to the lake root and use `/`: `<table>/data/<file>.parquet`,
//! `<table>/_delta_log/<version>.json`, `watermark-<consumer>-<table>.txt`. A local
//! directory maps them onto paths; a bucket maps them onto object keys.
//!
//! Two differences between the two are worth stating rather than hiding:
//!
//! - **No directories.** An object store has no empty directory, so `put` creating a
//!   parent is a local-only detail, and a "directory" of a lake is whatever its keys say.
//! - **No rename.** A commit is written once, under its final name, with a create-only
//!   write ([`LakeStore::put_new`]). Locally that is a rename from a scratch name; on S3 it
//!   is a conditional PUT. Either way a reader listing `NNN.json` never sees a half-written
//!   commit, and a second writer cannot overwrite the first.

use object_store::{ObjectStore as _, ObjectStoreExt as _};
use std::path::{Path, PathBuf};

/// One object in the lake: its key relative to the lake root, and its size in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
}

/// The bytes behind a lake. Synchronous on purpose: every verb in this crate is a batch
/// job or a step inside one, and an async lake would push a runtime into callers that only
/// wanted to write a file (see [`S3`] for where the async lives instead).
pub trait LakeStore: Send + Sync {
    /// Read a key whole. Absence is an error: a caller that expects absence asks [`head`].
    ///
    /// [`head`]: LakeStore::head
    fn get(&self, path: &str) -> Result<Vec<u8>, String>;
    /// Write a key, replacing whatever is there.
    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// Write a key only if it is absent, answering whether this call wrote it. This is how
    /// a Delta version is claimed: the version number is the writer's, and a version that
    /// already exists is somebody else's.
    fn put_new(&self, path: &str, bytes: &[u8]) -> Result<bool, String>;
    /// Everything under `prefix` (a key prefix, not a directory), in no particular order.
    fn list(&self, prefix: &str) -> Result<Vec<Entry>, String>;
    /// A key's size, or `None` when it is absent.
    fn head(&self, path: &str) -> Result<Option<Entry>, String>;
    /// Remove a key. Absence is not an error: the caller's intent is the key is gone.
    fn delete(&self, path: &str) -> Result<(), String>;
    /// What this lake is, for a report or an error message.
    fn describe(&self) -> String;
    /// The directory behind this lake, when it is a directory. A fold reads a lake by
    /// listing it, so nothing needs this; a caller that wants to hand the path to another
    /// program (a query engine, a test) does.
    fn local_root(&self) -> Option<PathBuf> {
        None
    }
}

/// A lake that is a directory.
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn at(&self, path: &str) -> PathBuf {
        let mut out = self.root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            out.push(part);
        }
        out
    }
}

impl LakeStore for LocalFs {
    fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        let full = self.at(path);
        std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let full = self.at(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        // Beside the target, so the rename below stays on one filesystem.
        let scratch = full.with_extension(format!(
            "{}.tmp",
            full.extension().and_then(|e| e.to_str()).unwrap_or("part")
        ));
        std::fs::write(&scratch, bytes).map_err(|e| format!("{}: {e}", scratch.display()))?;
        std::fs::rename(&scratch, &full).map_err(|e| format!("{}: {e}", full.display()))
    }

    fn put_new(&self, path: &str, bytes: &[u8]) -> Result<bool, String> {
        let full = self.at(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        // `create_new` is the atomic claim: it fails rather than overwrite.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)
                    .map_err(|e| format!("{}: {e}", full.display()))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(format!("{}: {e}", full.display())),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>, String> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, prefix, &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn head(&self, path: &str) -> Result<Option<Entry>, String> {
        let full = self.at(path);
        match std::fs::metadata(&full) {
            Ok(meta) if meta.is_file() => Ok(Some(Entry {
                path: path.to_string(),
                size: meta.len(),
            })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", full.display())),
        }
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        let full = self.at(path);
        match std::fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", full.display())),
        }
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn local_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Entry>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A lake directory that does not exist yet is an empty lake, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, prefix, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !rel.starts_with(prefix) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Entry { path: rel, size });
    }
    Ok(())
}

/// What a bucket needs that a directory does not: an endpoint, a region, and a key pair.
/// The secret is passed in, never read from a config file by this crate (ADR-0007's rule is
/// the caller's: a config names the environment variable, the process resolves it).
#[derive(Debug, Clone, Default)]
pub struct S3Options {
    /// A MinIO/rustfs-style endpoint (`http://host:9000`). `None` = AWS.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key: String,
    pub secret_key: String,
}

impl S3Options {
    /// Credentials from the environment: the `AWS_*` names first (what every tool and SDK
    /// already reads), then this workspace's `OKM_S3_*` names for a local rustfs.
    ///
    /// A config still names the variables; this is where the process resolves them, which is
    /// the split ADR-0007 asks for — no secret is stored by the crate.
    pub fn from_env() -> Self {
        let read = |aws: &str, okm: &str| {
            std::env::var(aws)
                .or_else(|_| std::env::var(okm))
                .ok()
                .filter(|value| !value.is_empty())
        };
        Self {
            endpoint: read("AWS_ENDPOINT_URL", "OKM_S3_ENDPOINT"),
            region: read("AWS_REGION", "OKM_S3_REGION"),
            access_key: read("AWS_ACCESS_KEY_ID", "OKM_S3_ACCESS_KEY").unwrap_or_default(),
            secret_key: read("AWS_SECRET_ACCESS_KEY", "OKM_S3_SECRET").unwrap_or_default(),
        }
    }
}

/// A lake that is a bucket, behind a synchronous face.
///
/// The runtime is this handle's own and is current-threaded: the verbs call it from a
/// blocking thread, one operation at a time, which is what a batch writer does anyway. A
/// caller inside an async runtime calls these verbs from `spawn_blocking`, never from the
/// runtime's own thread.
pub struct S3 {
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    rt: tokio::runtime::Runtime,
    /// The key prefix inside the bucket, without its trailing slash (may be empty).
    prefix: String,
    label: String,
}

impl S3 {
    /// Build a bucket-backed lake from `bucket`/`prefix` and credentials.
    pub fn new(bucket: &str, prefix: &str, options: &S3Options) -> Result<Self, String> {
        use object_store::aws::AmazonS3Builder;
        let mut builder = AmazonS3Builder::new().with_bucket_name(bucket);
        if let Some(endpoint) = &options.endpoint {
            builder = builder.with_endpoint(endpoint.clone());
            // A local endpoint speaks plain HTTP; AWS itself never asks for it.
            if endpoint.starts_with("http://") {
                builder = builder.with_allow_http(true);
            }
            // Path-style addressing is what MinIO-family servers answer on.
            builder = builder.with_virtual_hosted_style_request(false);
        }
        if !options.access_key.is_empty() {
            builder = builder.with_access_key_id(options.access_key.clone());
        }
        if !options.secret_key.is_empty() {
            builder = builder.with_secret_access_key(options.secret_key.clone());
        }
        if let Some(region) = &options.region {
            builder = builder.with_region(region.clone());
        }
        let store = builder.build().map_err(|e| format!("s3 {bucket}: {e}"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("s3 runtime: {e}"))?;
        let prefix = prefix.trim_matches('/').to_string();
        let label = if prefix.is_empty() {
            format!("s3://{bucket}")
        } else {
            format!("s3://{bucket}/{prefix}")
        };
        Ok(Self {
            store: std::sync::Arc::new(store),
            rt,
            prefix,
            label,
        })
    }

    /// The object key for a lake key.
    fn key(&self, path: &str) -> object_store::path::Path {
        let joined = if self.prefix.is_empty() {
            path.to_string()
        } else {
            format!("{}/{path}", self.prefix)
        };
        object_store::path::Path::from(joined)
    }
}

impl LakeStore for S3 {
    fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        let key = self.key(path);
        let store = self.store.clone();
        self.rt.block_on(async move {
            let got = store.get(&key).await.map_err(|e| format!("{}: {e}", key))?;
            let bytes = got.bytes().await.map_err(|e| format!("{}: {e}", key))?;
            Ok(bytes.to_vec())
        })
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let key = self.key(path);
        let store = self.store.clone();
        let payload = object_store::PutPayload::from_bytes(bytes::Bytes::copy_from_slice(bytes));
        self.rt.block_on(async move {
            store
                .put(&key, payload)
                .await
                .map(|_| ())
                .map_err(|e| format!("{}: {e}", key))
        })
    }

    fn put_new(&self, path: &str, bytes: &[u8]) -> Result<bool, String> {
        use object_store::{PutMode, PutOptions};
        let key = self.key(path);
        let store = self.store.clone();
        let payload = object_store::PutPayload::from_bytes(bytes::Bytes::copy_from_slice(bytes));
        let options = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        self.rt.block_on(async move {
            match store.put_opts(&key, payload, options).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                // A server that cannot express "create only" answers 412 instead; the
                // distinction does not matter to the caller, both mean "not mine".
                Err(object_store::Error::Precondition { .. }) => Ok(false),
                Err(e) => Err(format!("{}: {e}", key)),
            }
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>, String> {
        use futures::StreamExt;
        let key = self.key(prefix);
        let store = self.store.clone();
        let label = self.label.clone();
        let strip = self.prefix.clone();
        self.rt.block_on(async move {
            let mut out = Vec::new();
            let mut stream = store.list(Some(&key));
            while let Some(item) = stream.next().await {
                let meta = item.map_err(|e| format!("{label}: {e}"))?;
                out.push(meta);
            }
            let entries = out
                .into_iter()
                .map(|meta| {
                    let text = meta.location.to_string();
                    let path = if strip.is_empty() {
                        text.clone()
                    } else {
                        text.strip_prefix(&format!("{strip}/"))
                            .unwrap_or(&text)
                            .to_string()
                    };
                    Entry {
                        path,
                        size: meta.size,
                    }
                })
                .collect();
            Ok(entries)
        })
    }

    fn head(&self, path: &str) -> Result<Option<Entry>, String> {
        let key = self.key(path);
        let store = self.store.clone();
        let rel = path.to_string();
        self.rt.block_on(async move {
            match store.head(&key).await {
                Ok(meta) => Ok(Some(Entry {
                    path: rel,
                    size: meta.size,
                })),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(format!("{}: {e}", key)),
            }
        })
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        let key = self.key(path);
        let store = self.store.clone();
        self.rt.block_on(async move {
            match store.delete(&key).await {
                Ok(()) => Ok(()),
                Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(format!("{}: {e}", key)),
            }
        })
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

/// Open a lake from a URL: `s3://bucket/prefix` needs credentials, anything else is a
/// directory (a plain path or `file://`).
pub fn open(url: &str, options: Option<S3Options>) -> Result<Box<dyn LakeStore>, String> {
    open_url(&LakeUrl::parse(url)?, options)
}

/// A lake's address, parsed: a key space or a directory.
///
/// Parsing is separate from opening because the FORM is what a caller configures and what a
/// test can assert: no bucket is contacted here, and `s3://` is one enum arm rather than a
/// string a verb has to keep re-reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LakeUrl {
    /// A directory on this machine — `path` or `file://path`.
    Dir(PathBuf),
    /// A bucket, with the key prefix the lake sits under (may be empty).
    S3 { bucket: String, prefix: String },
}

impl LakeUrl {
    pub fn parse(url: &str) -> Result<Self, String> {
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((bucket, prefix)) => (bucket, prefix.to_string()),
                None => (rest, String::new()),
            };
            if bucket.is_empty() {
                return Err(format!("`{url}` names no bucket"));
            }
            return Ok(Self::S3 {
                bucket: bucket.to_string(),
                prefix: prefix.trim_matches('/').to_string(),
            });
        }
        let path = url.strip_prefix("file://").unwrap_or(url);
        if path.is_empty() {
            return Err(format!("`{url}` names no directory"));
        }
        Ok(Self::Dir(PathBuf::from(path)))
    }

    /// How this lake is named in a report or an error.
    pub fn describe(&self) -> String {
        match self {
            Self::Dir(path) => path.display().to_string(),
            Self::S3 { bucket, prefix } if prefix.is_empty() => format!("s3://{bucket}"),
            Self::S3 { bucket, prefix } => format!("s3://{bucket}/{prefix}"),
        }
    }
}

impl std::str::FromStr for LakeUrl {
    type Err = String;
    fn from_str(url: &str) -> Result<Self, String> {
        Self::parse(url)
    }
}

/// [`open`] for an already-parsed address.
pub fn open_url(url: &LakeUrl, options: Option<S3Options>) -> Result<Box<dyn LakeStore>, String> {
    match url {
        LakeUrl::S3 { bucket, prefix } => {
            let options = options.unwrap_or_default();
            Ok(Box::new(S3::new(bucket, prefix, &options)?))
        }
        LakeUrl::Dir(path) => Ok(Box::new(LocalFs::new(path))),
    }
}

/// A lake's key for `<table>/<relative>`.
pub fn table_key(table: &str, relative: &str) -> String {
    format!(
        "{}/{}",
        table.trim_end_matches('/'),
        relative.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_lake_round_trips_its_keys() {
        let dir = tempfile::tempdir().unwrap();
        let lake = LocalFs::new(dir.path());
        lake.put("0047/data/a.parquet", b"bytes").unwrap();
        assert_eq!(lake.get("0047/data/a.parquet").unwrap(), b"bytes");
        let listed = lake.list("0047/").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "0047/data/a.parquet");
        assert_eq!(listed[0].size, 5);
        assert_eq!(lake.head("0047/data/a.parquet").unwrap().unwrap().size, 5);
        assert!(lake.head("0047/data/missing.parquet").unwrap().is_none());
        lake.delete("0047/data/a.parquet").unwrap();
        // Deleting what is gone is the caller's intent, not an error.
        lake.delete("0047/data/a.parquet").unwrap();
        assert!(lake.list("").unwrap().is_empty());
    }

    #[test]
    fn a_version_is_claimed_once() {
        let dir = tempfile::tempdir().unwrap();
        let lake = LocalFs::new(dir.path());
        assert!(
            lake.put_new("0047/_delta_log/00000000000000000000.json", b"1")
                .unwrap()
        );
        assert!(
            !lake
                .put_new("0047/_delta_log/00000000000000000000.json", b"2")
                .unwrap()
        );
        assert_eq!(
            lake.get("0047/_delta_log/00000000000000000000.json")
                .unwrap(),
            b"1"
        );
    }

    #[test]
    fn a_plain_path_and_a_file_url_open_the_same_place() {
        let dir = tempfile::tempdir().unwrap();
        let plain = open(&dir.path().display().to_string(), None).unwrap();
        let url = open(&format!("file://{}", dir.path().display()), None).unwrap();
        assert_eq!(plain.describe(), url.describe());
        assert_eq!(plain.local_root(), Some(dir.path().to_path_buf()));
    }

    #[test]
    fn a_bucket_url_with_no_bucket_is_refused() {
        assert!(open("s3:///lake", None).is_err());
    }
}
