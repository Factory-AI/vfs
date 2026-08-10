//! Remote-tier wire vocabulary and object-store access.

mod store;
pub(crate) mod streamer;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vfs_core::{error::Error as VfsError, ChunkSource};

pub use store::RemoteStore;

/// CLI-owned object-store adapter for core chunk resolution.
pub(crate) struct RemoteChunkSource {
    store: RemoteStore,
    remote_url: String,
}

impl RemoteChunkSource {
    pub(crate) fn new(remote_url: &str) -> Result<Self> {
        Ok(Self {
            store: RemoteStore::new(remote_url)?,
            remote_url: remote_url.to_string(),
        })
    }
}

#[async_trait]
impl ChunkSource for RemoteChunkSource {
    async fn fetch(&self, digest: &[u8; 32]) -> vfs_core::error::Result<Vec<u8>> {
        let digest_hex = hex::encode(digest);
        self.store
            .get(&chunk_key(digest))
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| {
                VfsError::Internal(format!(
                    "failed to fetch remote chunk {digest_hex} from {}: {error:#}",
                    self.remote_url
                ))
            })
    }
}

/// Path of the persisted object-store locator for an installed session.
pub(crate) fn remote_url_path(run_dir: &Path) -> PathBuf {
    run_dir.join("remote")
}

/// Publish the object-store locator before the session database commit point.
pub(crate) fn write_remote_url(run_dir: &Path, remote_url: &str) -> Result<()> {
    if remote_url.trim().is_empty() {
        anyhow::bail!("remote session URL must not be empty");
    }
    let path = remote_url_path(run_dir);
    fs::write(&path, remote_url.as_bytes())
        .with_context(|| format!("Failed to publish remote session URL {}", path.display()))?;
    super::pack::sync_file_and_parent(&path)
}

/// Read the object-store locator persisted by remote adoption.
pub(crate) fn read_remote_url(run_dir: &Path) -> Result<Option<String>> {
    let path = remote_url_path(run_dir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read remote session URL {}", path.display()));
        }
    };
    let remote_url = contents.trim();
    if remote_url.is_empty() {
        anyhow::bail!("remote session URL {} is empty", path.display());
    }
    Ok(Some(remote_url.to_string()))
}

/// CLI-edge configuration for the remote tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub url: String,
    pub concurrency: usize,
    pub stream_interval_ms: u64,
}

/// Location and integrity metadata for the hollowed metadata artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMetadata {
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
}

/// One-line, additive remote checkpoint manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteManifest {
    pub session_id: String,
    pub head_seq: i64,
    pub history_epoch: i64,
    pub history_valid: bool,
    pub generation: u64,
    pub artifact_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_pin: Option<String>,
    pub metadata: RemoteMetadata,
    pub chunk_count: u64,
    pub chunk_bytes: u64,
    pub created_at_ms: i64,
    pub vfs_version: String,
}

impl RemoteManifest {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize remote manifest")
    }

    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("Failed to parse remote manifest")
    }
}

/// Canonical content-addressed chunk object key.
pub fn chunk_key(digest: &[u8; 32]) -> String {
    format!("chunks/{}", hex::encode(digest))
}

/// Canonical per-session manifest object key.
pub fn manifest_key(session_id: &str) -> String {
    format!("sessions/{session_id}/manifest.json")
}

/// Canonical hollowed metadata artifact object key.
pub fn metadata_key(session_id: &str, sha256_hex: &str) -> String {
    format!("sessions/{session_id}/meta/{sha256_hex}.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use url::Url;

    #[test]
    fn remote_key_layout_is_pinned() {
        assert_eq!(
            chunk_key(&[0xab; 32]),
            "chunks/abababababababababababababababababababababababababababababababab"
        );
        assert_eq!(
            manifest_key("session-123"),
            "sessions/session-123/manifest.json"
        );
        assert_eq!(
            metadata_key(
                "session-123",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            "sessions/session-123/meta/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.db"
        );
    }

    #[test]
    fn remote_manifest_json_round_trips_on_one_line() {
        let manifest = RemoteManifest {
            session_id: "session-123".to_string(),
            head_seq: 42,
            history_epoch: 7,
            history_valid: true,
            generation: 3,
            artifact_version: "1".to_string(),
            seed_pin: Some("0123456789abcdef".to_string()),
            metadata: RemoteMetadata {
                key: metadata_key("session-123", "feedface"),
                sha256: "feedface".to_string(),
                bytes: 8192,
            },
            chunk_count: 4,
            chunk_bytes: 16_384,
            created_at_ms: 1_763_000_000_000,
            vfs_version: "1.0.2".to_string(),
        };

        let json = manifest.to_json().unwrap();
        assert!(!json.contains('\n'));
        assert!(json.contains("\"sessionId\":\"session-123\""));
        assert!(json.contains("\"createdAtMs\":1763000000000"));
        assert_eq!(RemoteManifest::from_json(&json).unwrap(), manifest);

        let mut extended: serde_json::Value = serde_json::from_str(&json).unwrap();
        extended
            .as_object_mut()
            .unwrap()
            .insert("futureField".to_string(), serde_json::json!("ignored"));
        assert_eq!(
            RemoteManifest::from_json(&serde_json::to_string(&extended).unwrap()).unwrap(),
            manifest
        );
    }

    #[test]
    fn remote_url_sidecar_round_trips() {
        let run_dir = tempfile::tempdir().unwrap();
        assert_eq!(read_remote_url(run_dir.path()).unwrap(), None);

        write_remote_url(run_dir.path(), "file:///tmp/vfs-remote").unwrap();
        assert_eq!(
            read_remote_url(run_dir.path()).unwrap().as_deref(),
            Some("file:///tmp/vfs-remote")
        );
        assert_eq!(
            remote_url_path(run_dir.path()),
            run_dir.path().join("remote")
        );
    }

    #[test]
    fn remote_url_sidecar_refuses_empty_content() {
        let run_dir = tempfile::tempdir().unwrap();
        fs::write(remote_url_path(run_dir.path()), " \n").unwrap();
        let error = read_remote_url(run_dir.path()).unwrap_err();
        assert!(error.to_string().contains("is empty"));
    }

    #[tokio::test]
    async fn remote_chunk_source_fetches_file_objects() {
        let remote = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(remote.path()).unwrap().to_string();
        let source = RemoteChunkSource::new(&url).unwrap();
        let bytes = b"remote chunk bytes";
        let digest = *blake3::hash(bytes).as_bytes();
        source
            .store
            .put(&chunk_key(&digest), Bytes::copy_from_slice(bytes))
            .await
            .unwrap();

        assert_eq!(source.fetch(&digest).await.unwrap(), bytes);
    }

    #[tokio::test]
    async fn remote_chunk_source_translates_fetch_errors_with_digest_context() {
        let remote = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(remote.path()).unwrap().to_string();
        let source = RemoteChunkSource::new(&url).unwrap();
        let digest = [0x7a; 32];

        let error = source.fetch(&digest).await.unwrap_err();
        assert!(matches!(error, VfsError::Internal(_)));
        let message = error.to_string();
        assert!(message.contains(&hex::encode(digest)));
        assert!(message.contains(&url));
        assert!(message.contains("Failed to get remote object"));
    }
}
