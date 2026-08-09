//! Remote-tier wire vocabulary and object-store access.

mod store;
pub(crate) mod streamer;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use store::RemoteStore;

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
}
