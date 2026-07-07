use std::path::{Path, PathBuf};

use turso::sync::PartialSyncOpts;

use crate::config::CoreConfig;
use crate::error::{Error, Result};

/// Directory containing agentfs databases
pub fn agentfs_dir() -> &'static std::path::Path {
    std::path::Path::new(".agentfs")
}

/// Configuration options for sync
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Remote URL for syncing
    pub remote_url: Option<String>,
    /// Auth token for remote sync
    pub auth_token: Option<String>,
    /// Partial sync options
    pub partial_sync: Option<PartialSyncOpts>,
}

/// Configuration options for local encryption
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    /// Hex-encoded encryption key
    pub hex_key: String,
    /// Cipher algorithm (e.g., "aegis256", "aegis128l", "aes256gcm" etc)
    pub cipher: String,
}

/// Configuration options for opening an AgentFS instance
#[derive(Debug, Clone, Default)]
pub struct AgentFSOptions {
    /// Optional unique identifier for the agent.
    /// - If Some(id): Creates persistent storage at `.agentfs/{id}.db`
    /// - If None: Uses ephemeral in-memory database
    pub id: Option<String>,
    /// Optional custom path to the database file.
    /// Takes precedence over `id` if both are set.
    pub path: Option<String>,
    /// Optional base directory for overlay filesystem (copy-on-write).
    /// When set, the filesystem operates as an overlay on top of this directory.
    pub base: Option<PathBuf>,
    /// Sync options for remote database synchronization
    pub sync: SyncOptions,
    /// Encryption configuration for database at rest
    pub encryption: Option<EncryptionConfig>,
    /// Typed core runtime configuration. When omitted, [`CoreConfig::from_env`]
    /// is evaluated once by [`AgentFS::open`](crate::AgentFS::open).
    pub core_config: Option<CoreConfig>,
}

impl AgentFSOptions {
    /// Validates an agent ID to prevent path traversal and ensure safe filesystem operations.
    /// Returns true if the ID contains only alphanumeric characters, hyphens, and underscores.
    pub fn validate_agent_id(id: &str) -> bool {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }
    pub fn db_path(&self) -> Result<String> {
        // Determine database path: path takes precedence over id
        let path = if let Some(path) = &self.path {
            // Custom path provided directly
            if path == ":memory:" {
                return Ok(path.clone());
            }
            PathBuf::from(path)
        } else if let Some(id) = &self.id {
            // Validate agent ID to prevent path traversal attacks
            if !Self::validate_agent_id(id) {
                return Err(Error::InvalidAgentId(id.clone()));
            }

            // Ensure .agentfs directory exists
            let agentfs_dir = agentfs_dir();
            if !agentfs_dir.exists() {
                std::fs::create_dir_all(agentfs_dir)?;
            }
            agentfs_dir.join(format!("{}.db", id))
        } else {
            // No id or path = ephemeral in-memory database
            return Ok(":memory:".to_string());
        };
        // turso retains this string verbatim for by-path operations, and mount
        // teardown chdirs the process to `/`; hand it an absolute path so
        // nothing downstream can resolve it against the wrong cwd.
        let path = std::path::absolute(path)?;
        path.into_os_string()
            .into_string()
            .map_err(|p| Error::InvalidUtf8Path(PathBuf::from(p).display().to_string()))
    }
    /// Create options for a persistent agent with the given ID
    pub fn with_id(id: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            path: None,
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
            core_config: None,
        }
    }

    /// Create options for an ephemeral in-memory agent
    pub fn ephemeral() -> Self {
        Self {
            id: None,
            path: None,
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
            core_config: None,
        }
    }

    /// Create options with a custom database path
    pub fn with_path(path: impl Into<String>) -> Self {
        Self {
            id: None,
            path: Some(path.into()),
            base: None,
            sync: SyncOptions::default(),
            encryption: None,
            core_config: None,
        }
    }

    /// Set sync options
    pub fn with_sync(mut self, sync: SyncOptions) -> Self {
        self.sync = sync;
        self
    }

    /// Set typed core runtime configuration.
    pub fn with_core_config(mut self, core_config: CoreConfig) -> Self {
        self.core_config = Some(core_config);
        self
    }

    /// Set the base directory for overlay filesystem (copy-on-write)
    pub fn with_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.base = Some(base.into());
        self
    }

    /// Enable local encryption with a hex-encoded key and cipher
    ///
    /// # Arguments
    /// * `hex_key` - Hex-encoded encryption key (64 chars for 256-bit ciphers, 32 for 128-bit)
    /// * `cipher` - Cipher algorithm (e.g., "aegis256", "aes256gcm" etc)
    pub fn with_encryption_key(mut self, hex_key: &str, cipher: &str) -> Self {
        self.encryption = Some(EncryptionConfig {
            hex_key: hex_key.to_string(),
            cipher: cipher.to_string(),
        });
        self
    }

    /// Set encryption configuration directly
    pub fn with_encryption(mut self, encryption: EncryptionConfig) -> Self {
        self.encryption = Some(encryption);
        self
    }

    /// Resolve an id-or-path string to AgentFSOptions
    ///
    /// Resolution order (first match wins):
    /// 1. `:memory:` -> ephemeral in-memory database
    /// 2. Valid agent ID with existing `.agentfs/{id}.db` -> uses that agent
    /// 3. Existing file path -> uses that path directly
    ///
    /// When nothing matches: ID-shaped arguments report `AgentNotFound`,
    /// path-shaped arguments (a separator or an extension) report
    /// `DatabaseNotFound`, and everything else `InvalidAgentId`.
    pub fn resolve(id_or_path: impl Into<String>) -> Result<Self> {
        let id_or_path = id_or_path.into();

        if id_or_path == ":memory:" {
            return Ok(Self::ephemeral());
        }

        // First, check if it's a valid agent ID with an existing database in .agentfs/
        if AgentFSOptions::validate_agent_id(&id_or_path) {
            let db_path = agentfs_dir().join(format!("{}.db", id_or_path));
            if db_path.exists() {
                return Ok(Self::with_path(db_path.to_str().ok_or_else(|| {
                    Error::InvalidUtf8Path(db_path.display().to_string())
                })?));
            }
        }

        // Fall back to treating as a direct file path
        let path = Path::new(&id_or_path);
        if path.is_file() {
            Ok(Self::with_path(id_or_path))
        } else if AgentFSOptions::validate_agent_id(&id_or_path) {
            // Not a valid agent and not an existing file
            Err(Error::AgentNotFound {
                id: id_or_path.clone(),
                path: agentfs_dir()
                    .join(format!("{}.db", id_or_path))
                    .display()
                    .to_string(),
            })
        } else if id_or_path.contains(std::path::MAIN_SEPARATOR) || path.extension().is_some() {
            // Path-shaped (has a separator or an extension) but nothing exists
            // there: an "invalid agent ID" complaint would be misleading.
            Err(Error::DatabaseNotFound(id_or_path))
        } else {
            Err(Error::InvalidAgentId(id_or_path))
        }
    }
}
