use agentfs_core::fs::{FileSystem, DEFAULT_FILE_MODE};
use agentfs_core::{AgentFS, AgentFSOptions, Stats};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cmd::init::{finalize_readonly, open_agentfs};

/// The complete dispatchable tool surface. `tools/list`, `tools/call`
/// dispatch, and `--tools` filter validation all key off this list; the
/// parity tests in `mcp_server/tests.rs` keep it honest.
const ALL_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "readdir",
    "mkdir",
    "remove",
    "rename",
    "stat",
    "access",
    "kv_get",
    "kv_set",
    "kv_delete",
    "kv_list",
];

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC error with the code it must be reported under.
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("Unknown method: {method}"),
        }
    }

    fn invalid_params(message: String) -> Self {
        Self {
            code: INVALID_PARAMS,
            message,
        }
    }

    fn internal(message: String) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message,
        }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(format!("{err:#}"))
    }
}

fn parse_tool_params<T: serde::de::DeserializeOwned>(arguments: JsonValue) -> Result<T, RpcError> {
    serde_json::from_value(arguments)
        .map_err(|err| RpcError::invalid_params(format!("Invalid tool arguments: {err}")))
}

/// Main entry point for MCP server command
pub async fn handle_mcp_server_command(
    id_or_path: String,
    tools_filter: Option<Vec<String>>,
) -> Result<()> {
    // Resolve and open AgentFS
    let options = AgentFSOptions::resolve(&id_or_path).context(format!(
        "Failed to resolve agent ID or path: {}",
        id_or_path
    ))?;

    eprintln!("Using agent: {}", id_or_path);

    // Create MCP server with tool filtering
    let server = McpServer::new(id_or_path, options, tools_filter).await?;

    // Run server with stdio transport
    eprintln!("Starting MCP server on stdio...");
    eprintln!("Protocol: Model Context Protocol (MCP) over JSON-RPC 2.0");
    server.serve().await?;

    Ok(())
}

/// Where request handlers get their AgentFS from.
enum AgentFsSource {
    /// Ephemeral (in-memory) databases live exactly as long as this handle.
    Held(Arc<AgentFS>),
    /// File-backed databases are reopened per request. The database file
    /// lock is exclusive (one process at a time), so holding it across the
    /// whole stdio session would block every other CLI command (including
    /// `agentfs timeline` reading the tool audit) for as long as a client
    /// keeps the server open. Reopening per request releases the lock while
    /// the server is idle.
    PerRequest(Box<AgentFSOptions>),
}

/// MCP Server implementation
struct McpServer {
    /// The user-supplied database identity, kept for schema-mismatch
    /// guidance on the per-request reopens (the database can be replaced
    /// underneath a long-lived stdio session).
    id_or_path: String,
    source: AgentFsSource,
    enabled_tools: Option<HashSet<String>>,
}

impl McpServer {
    async fn new(
        id_or_path: String,
        options: AgentFSOptions,
        tools_filter: Option<Vec<String>>,
    ) -> Result<Self> {
        let enabled_tools = match tools_filter {
            None => {
                eprintln!("No tool filter specified. Exposing all tools.");
                None
            }
            Some(tools) => {
                let mut unknown: Vec<String> = tools
                    .iter()
                    .filter(|tool| !ALL_TOOLS.contains(&tool.as_str()))
                    .cloned()
                    .collect();
                unknown.sort();
                unknown.dedup();
                if !unknown.is_empty() {
                    anyhow::bail!(
                        "unknown tool(s) in --tools filter: {}. Available tools: {}",
                        unknown.join(", "),
                        ALL_TOOLS.join(", ")
                    );
                }
                let set: HashSet<String> = tools.into_iter().collect();
                if set.is_empty() {
                    anyhow::bail!(
                        "--tools filter selected no tools. Available tools: {}",
                        ALL_TOOLS.join(", ")
                    );
                }
                eprintln!("Tool filter enabled. Exposing tools: {:?}", set);
                Some(set)
            }
        };

        let source = if options.db_path()? == ":memory:" {
            let agentfs = open_agentfs(options)
                .await
                .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;
            AgentFsSource::Held(Arc::new(agentfs))
        } else {
            // Open once up front so startup fails cleanly on a missing or
            // incompatible database, then release the file lock with the
            // single-file family restored: even this never-writing probe
            // materializes a -wal sidecar (invariant I1).
            let probe = open_agentfs(options.clone())
                .await
                .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;
            finalize_readonly(&probe).await;
            drop(probe);
            AgentFsSource::PerRequest(Box::new(options))
        };

        Ok(Self {
            id_or_path,
            source,
            enabled_tools,
        })
    }

    async fn agentfs(&self) -> Result<Arc<AgentFS>> {
        match &self.source {
            AgentFsSource::Held(agentfs) => Ok(agentfs.clone()),
            AgentFsSource::PerRequest(options) => {
                let agentfs = open_agentfs((**options).clone()).await.map_err(|err| {
                    super::migrate::open_error_with_guidance(err, &self.id_or_path)
                })?;
                Ok(Arc::new(agentfs))
            }
        }
    }

    /// Check if a tool is enabled based on filter
    fn is_tool_enabled(&self, tool_name: &str) -> bool {
        match &self.enabled_tools {
            None => true, // No filter = all enabled
            Some(set) => set.contains(tool_name),
        }
    }

    async fn serve(self) -> Result<()> {
        let server = Arc::new(Mutex::new(self));
        let result = Self::serve_stdio(&server).await;
        // Per-request opens leave a -wal next to a file-backed database
        // while serving; restore the single-file family at exit even when
        // the stdio loop errored, mirroring the NFS server's
        // finalize-on-shutdown (agentfs-nfs server/tcp.rs).
        server.lock().await.finalize_on_shutdown().await;
        result
    }

    /// Restore the single-file database family before the server exits.
    /// Best-effort like every one-shot command's finalize: a concurrent
    /// holder of the database must not turn a clean shutdown into an error.
    async fn finalize_on_shutdown(&self) {
        // Ephemeral databases have no on-disk family to restore.
        let AgentFsSource::PerRequest(options) = &self.source else {
            return;
        };
        match open_agentfs((**options).clone())
            .await
            .map_err(|err| super::migrate::open_error_with_guidance(err, &self.id_or_path))
        {
            Ok(agentfs) => finalize_readonly(&agentfs).await,
            Err(error) => eprintln!(
                "Warning: Failed to reopen the database to restore the single-file family: {error:#}"
            ),
        }
    }

    async fn serve_stdio(server: &Arc<Mutex<Self>>) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            // Parse JSON-RPC request
            let request: JsonValue = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(e) => {
                    eprintln!("Failed to parse JSON-RPC request: {}", e);
                    continue;
                }
            };

            // Handle request
            let response = server.lock().await.handle_request(request).await;

            // Write response to stdout
            if let Some(resp) = response {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{}", resp_str)?;
                stdout.flush()?;
            }
        }

        Ok(())
    }

    async fn handle_request(&self, request: JsonValue) -> Option<JsonValue> {
        // Extract request fields
        let method = request.get("method")?.as_str()?;
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        eprintln!("Received request: method={}", method);

        // Handle method
        let result = match method {
            "initialize" => self.handle_initialize(params).await.map_err(RpcError::from),
            // Spec notifications (e.g. notifications/initialized) are
            // acknowledged by ignoring them; they must never get a response.
            "initialized" => return None,
            method if method.starts_with("notifications/") => return None,
            "tools/list" => self.handle_tools_list().await.map_err(RpcError::from),
            "tools/call" => self.handle_tools_call(params).await,
            "resources/list" => self.handle_resources_list().await.map_err(RpcError::from),
            "resources/read" => self
                .handle_resources_read(params)
                .await
                .map_err(RpcError::from),
            _ => Err(RpcError::method_not_found(method)),
        };

        // JSON-RPC requests without an id are notifications: process them
        // above, but never respond (not even with an error).
        let id = id.filter(|id| !id.is_null())?;

        // Build JSON-RPC response
        let response = match result {
            Ok(result) => {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                })
            }
            Err(e) => {
                eprintln!("Error handling {}: {}", method, e.message);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": e.code,
                        "message": e.message
                    }
                })
            }
        };

        Some(response)
    }

    async fn handle_initialize(&self, _params: JsonValue) -> Result<JsonValue> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "agentfs-mcp-server",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    async fn handle_tools_list(&self) -> Result<JsonValue> {
        let tools = self.get_tool_definitions();
        Ok(json!({ "tools": tools }))
    }

    async fn handle_tools_call(&self, params: JsonValue) -> Result<JsonValue, RpcError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcError::invalid_params("Missing tool name".to_string()))?;

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        let agentfs = self
            .agentfs()
            .await
            .map_err(|err| RpcError::internal(format!("Failed to open database: {err:#}")))?;

        // Every tools/call attempt is audited, including rejected ones, so
        // `agentfs timeline` shows the full MCP activity for the database.
        let audit_id = agentfs
            .tools
            .start(name, Some(arguments.clone()))
            .await
            .map_err(|err| {
                RpcError::internal(format!("Failed to record tool call audit row: {err}"))
            })?;

        let outcome = self.dispatch_tool(&agentfs, name, arguments).await;

        match &outcome {
            Ok(text) => {
                agentfs
                    .tools
                    .success(audit_id, Some(JsonValue::String(text.clone())))
                    .await
            }
            Err(err) => agentfs.tools.error(audit_id, &err.message).await,
        }
        .map_err(|err| {
            RpcError::internal(format!("Failed to record tool call audit row: {err}"))
        })?;

        let result_text = outcome?;

        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": result_text
                }
            ]
        }))
    }

    async fn dispatch_tool(
        &self,
        agentfs: &AgentFS,
        name: &str,
        arguments: JsonValue,
    ) -> Result<String, RpcError> {
        if !ALL_TOOLS.contains(&name) {
            return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
        }
        if !self.is_tool_enabled(name) {
            return Err(RpcError::invalid_params(format!(
                "Tool not enabled by --tools filter: {name}"
            )));
        }

        let result = match name {
            "read_file" => handle_read_file(agentfs, parse_tool_params(arguments)?).await,
            "write_file" => handle_write_file(agentfs, parse_tool_params(arguments)?).await,
            "readdir" => handle_readdir(agentfs, parse_tool_params(arguments)?).await,
            "mkdir" => handle_mkdir(agentfs, parse_tool_params(arguments)?).await,
            "rename" => handle_rename(agentfs, parse_tool_params(arguments)?).await,
            "remove" => handle_remove(agentfs, parse_tool_params(arguments)?).await,
            "stat" => handle_stat(agentfs, parse_tool_params(arguments)?).await,
            "access" => handle_access(agentfs, parse_tool_params(arguments)?).await,
            "kv_get" => handle_kv_get(agentfs, parse_tool_params(arguments)?).await,
            "kv_set" => handle_kv_set(agentfs, parse_tool_params(arguments)?).await,
            "kv_delete" => handle_kv_delete(agentfs, parse_tool_params(arguments)?).await,
            "kv_list" => handle_kv_list(agentfs, parse_tool_params(arguments)?).await,
            _ => Err(anyhow::anyhow!(
                "Tool {name} is advertised but has no dispatch arm; \
                 ALL_TOOLS and dispatch_tool are out of sync"
            )),
        };

        result.map_err(RpcError::from)
    }

    async fn handle_resources_list(&self) -> Result<JsonValue> {
        let agentfs = self.agentfs().await?;
        let resources = list_resources(&agentfs).await?;
        Ok(json!({ "resources": resources }))
    }

    async fn handle_resources_read(&self, params: JsonValue) -> Result<JsonValue> {
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing resource uri"))?;

        let agentfs = self.agentfs().await?;
        let contents = read_resource(&agentfs, uri).await?;
        let mime_type = guess_mime_type(uri);

        // Try to decode as UTF-8, fall back to base64
        let resource_contents = if let Ok(text) = String::from_utf8(contents.clone()) {
            json!({
                "uri": uri,
                "mimeType": mime_type,
                "text": text
            })
        } else {
            json!({
                "uri": uri,
                "mimeType": mime_type,
                "blob": base64_encode(&contents)
            })
        };

        Ok(json!({ "contents": [resource_contents] }))
    }

    fn get_tool_definitions(&self) -> Vec<JsonValue> {
        let mut tools = Vec::new();

        // Filesystem tools
        if self.is_tool_enabled("read_file") {
            tools.push(json!({
                "name": "read_file",
                "description": "Read file contents from the filesystem",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read"
                        },
                        "encoding": {
                            "type": "string",
                            "enum": ["utf8", "base64"],
                            "description": "Encoding to use for file contents (default: utf8)"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("write_file") {
            tools.push(json!({
                "name": "write_file",
                "description": "Write content to a file in the filesystem. Existing files are overwritten in place and keep their mode; new files are created with mode 0644.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        },
                        "encoding": {
                            "type": "string",
                            "enum": ["utf8", "base64"],
                            "description": "Encoding of the content (default: utf8)"
                        },
                        "create_dirs": {
                            "type": "boolean",
                            "description": "Create parent directories if they don't exist"
                        }
                    },
                    "required": ["path", "content"]
                }
            }));
        }

        if self.is_tool_enabled("readdir") {
            tools.push(json!({
                "name": "readdir",
                "description": "List contents of a directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory to list"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("mkdir") {
            tools.push(json!({
                "name": "mkdir",
                "description": "Create a directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the directory to create"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("remove") {
            tools.push(json!({
                "name": "remove",
                "description": "Remove a file or empty directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the file or directory to remove"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("rename") {
            tools.push(json!({
                "name": "rename",
                "description": "Move or rename a file or directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "Source path"
                        },
                        "to": {
                            "type": "string",
                            "description": "Destination path"
                        }
                    },
                    "required": ["from", "to"]
                }
            }));
        }

        if self.is_tool_enabled("stat") {
            tools.push(json!({
                "name": "stat",
                "description": "Get file or directory metadata",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to stat"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("access") {
            tools.push(json!({
                "name": "access",
                "description": "Test if a path exists",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to test"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        // KV store tools
        if self.is_tool_enabled("kv_get") {
            tools.push(json!({
                "name": "kv_get",
                "description": "Get a value from the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to retrieve"
                        }
                    },
                    "required": ["key"]
                }
            }));
        }

        if self.is_tool_enabled("kv_set") {
            tools.push(json!({
                "name": "kv_set",
                "description": "Set a value in the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to set"
                        },
                        "value": {
                            "description": "Value to store (any JSON value)"
                        }
                    },
                    "required": ["key", "value"]
                }
            }));
        }

        if self.is_tool_enabled("kv_delete") {
            tools.push(json!({
                "name": "kv_delete",
                "description": "Delete a key from the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to delete"
                        }
                    },
                    "required": ["key"]
                }
            }));
        }

        if self.is_tool_enabled("kv_list") {
            tools.push(json!({
                "name": "kv_list",
                "description": "List keys in the key-value store with optional prefix",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prefix": {
                            "type": "string",
                            "description": "Optional prefix to filter keys"
                        }
                    }
                }
            }));
        }

        tools
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Normalize a path to ensure it starts with /
fn normalize_path(path: &str) -> Result<String> {
    let path = path.trim();

    // Reject paths with .. for security
    if path.contains("..") {
        anyhow::bail!("Path traversal not allowed: {}", path);
    }

    // Convert relative to absolute
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", path)
    };

    Ok(normalized)
}

/// Guess MIME type based on file extension
fn guess_mime_type(path: &str) -> String {
    match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("txt") | Some("md") => "text/plain",
        Some("json") => "application/json",
        Some("html") | Some("htm") => "text/html",
        Some("js") | Some("mjs") => "application/javascript",
        Some("ts") => "application/typescript",
        Some("css") => "text/css",
        Some("xml") => "application/xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Base64 encode bytes
fn base64_encode(data: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(data)
}

/// Base64 decode string
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD
        .decode(s)
        .map_err(|e| anyhow::anyhow!("Base64 decode error: {}", e))
}

// ============================================================================
// Tool parameter types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct ReadFileParams {
    path: String,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    create_dirs: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReaddirParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MkdirParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RenameParams {
    from: String,
    to: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoveParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StatParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AccessParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvGetParams {
    key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvSetParams {
    key: String,
    value: JsonValue,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvDeleteParams {
    key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvListParams {
    #[serde(default)]
    prefix: Option<String>,
}

// ============================================================================
// Tool implementations
// ============================================================================

/// Read file contents
async fn handle_read_file(agentfs: &AgentFS, params: ReadFileParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    let data = agentfs
        .fs
        .read_file(&path)
        .await
        .context("Failed to read file")?
        .ok_or_else(|| anyhow::anyhow!("File not found: {}", path))?;

    let content = match params.encoding.as_deref() {
        Some("base64") => base64_encode(&data),
        _ => String::from_utf8(data)
            .context("File is not valid UTF-8. Use encoding=base64 for binary files.")?,
    };

    Ok(content)
}

/// Write file contents.
///
/// Existing files are overwritten in place so they keep their inode and
/// mode; new files are created with the default mode (0644).
async fn handle_write_file(agentfs: &AgentFS, params: WriteFileParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    let data = match params.encoding.as_deref() {
        Some("base64") => base64_decode(&params.content)?,
        _ => params.content.into_bytes(),
    };

    // Create parent directories if requested
    if params.create_dirs.unwrap_or(false) {
        ensure_parent_dirs(agentfs, &path).await?;
    }

    let file = match agentfs.fs.stat(&path).await? {
        Some(stats) if stats.is_file() => {
            let file = FileSystem::open(&agentfs.fs, stats.ino, libc::O_WRONLY)
                .await
                .context("Failed to open file")?;
            file.truncate(0).await.context("Failed to truncate file")?;
            file
        }
        Some(_) => anyhow::bail!("Not a regular file: {}", path),
        None => {
            let (_, file) = agentfs
                .fs
                .create_file(&path, DEFAULT_FILE_MODE, 0, 0)
                .await
                .context("Failed to create file")?;
            file
        }
    };
    file.pwrite(0, &data)
        .await
        .context("Failed to write file")?;
    // Flush batched writes so the data is durable even if the server
    // exits right after this call (each MCP call is a complete exchange).
    file.drain_writes().await.context("Failed to flush file")?;

    Ok(format!("Wrote {} bytes to {}", data.len(), path))
}

/// Helper to create parent directories recursively
fn ensure_parent_dirs<'a>(
    agentfs: &'a AgentFS,
    path: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let path_obj = Path::new(path);
        let parent = match path_obj.parent() {
            Some(p) if !p.as_os_str().is_empty() && p != Path::new("/") => p,
            _ => return Ok(()),
        };

        let parent_str = parent.to_string_lossy().to_string();
        let parent_path = normalize_path(&parent_str)?;

        // Check if parent exists
        if agentfs.fs.stat(&parent_path).await?.is_some() {
            return Ok(());
        }

        // Recursively ensure grandparent exists
        ensure_parent_dirs(agentfs, &parent_path).await?;

        // Create parent
        agentfs
            .fs
            .mkdir(&parent_path, 0, 0)
            .await
            .context(format!("Failed to create directory: {}", parent_path))?;

        Ok(())
    })
}

/// List directory contents
async fn handle_readdir(agentfs: &AgentFS, params: ReaddirParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    let stats = agentfs
        .fs
        .stat(&path)
        .await
        .context("Failed to stat directory")?
        .ok_or_else(|| anyhow::anyhow!("Directory not found: {}", path))?;

    let entries = agentfs
        .fs
        .readdir(stats.ino)
        .await
        .context("Failed to read directory")?
        .ok_or_else(|| anyhow::anyhow!("Directory not found: {}", path))?;

    Ok(serde_json::to_string_pretty(&entries)?)
}

/// Create directory
async fn handle_mkdir(agentfs: &AgentFS, params: MkdirParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    agentfs
        .fs
        .mkdir(&path, 0, 0)
        .await
        .context("Failed to create directory")?;

    Ok(format!("Created directory: {}", path))
}

/// Remove empty directory
async fn handle_remove(agentfs: &AgentFS, params: RemoveParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    agentfs
        .fs
        .remove(&path)
        .await
        .context("Failed to remove directory")?;

    Ok(format!("Removed directory: {}", path))
}

/// Rename/move file or directory
async fn handle_rename(agentfs: &AgentFS, params: RenameParams) -> Result<String> {
    let from = normalize_path(&params.from)?;
    let to = normalize_path(&params.to)?;
    let (from_parent, from_name) = agentfs.fs.resolve_parent_and_name(&from).await?;
    let (to_parent, to_name) = agentfs.fs.resolve_parent_and_name(&to).await?;

    FileSystem::rename(&agentfs.fs, from_parent, &from_name, to_parent, &to_name)
        .await
        .context("Failed to rename")?;

    Ok(format!("Renamed {} to {}", from, to))
}

/// Get file metadata
async fn handle_stat(agentfs: &AgentFS, params: StatParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    let stats = agentfs
        .fs
        .stat(&path)
        .await
        .context("Failed to stat")?
        .ok_or_else(|| anyhow::anyhow!("Path not found: {}", path))?;

    Ok(serde_json::to_string_pretty(&StatsResponse::from(stats))?)
}

/// Test if path exists
async fn handle_access(agentfs: &AgentFS, params: AccessParams) -> Result<String> {
    let path = normalize_path(&params.path)?;

    let exists = agentfs.fs.stat(&path).await?.is_some();

    Ok(serde_json::to_string(&json!({ "exists": exists }))?)
}

/// Get KV value
async fn handle_kv_get(agentfs: &AgentFS, params: KvGetParams) -> Result<String> {
    let value: Option<JsonValue> = agentfs
        .kv
        .get(&params.key)
        .await
        .context("Failed to get value")?;

    Ok(serde_json::to_string_pretty(&value)?)
}

/// Set KV value
async fn handle_kv_set(agentfs: &AgentFS, params: KvSetParams) -> Result<String> {
    agentfs
        .kv
        .set(&params.key, &params.value)
        .await
        .context("Failed to set value")?;

    Ok(format!("Set key: {}", params.key))
}

/// Delete KV value
async fn handle_kv_delete(agentfs: &AgentFS, params: KvDeleteParams) -> Result<String> {
    agentfs
        .kv
        .delete(&params.key)
        .await
        .context("Failed to delete key")?;

    Ok(format!("Deleted key: {}", params.key))
}

/// List KV keys, optionally filtered by prefix
async fn handle_kv_list(agentfs: &AgentFS, params: KvListParams) -> Result<String> {
    let mut keys = agentfs.kv.keys().await.context("Failed to list keys")?;
    if let Some(prefix) = &params.prefix {
        keys.retain(|key| key.starts_with(prefix.as_str()));
    }
    keys.sort();

    Ok(serde_json::to_string_pretty(&keys)?)
}

/// List all files as resources
async fn list_resources(agentfs: &AgentFS) -> Result<Vec<JsonValue>> {
    let mut resources = Vec::new();
    collect_file_resources(agentfs, "/", &mut resources).await?;
    Ok(resources)
}

/// Recursively collect file resources
fn collect_file_resources<'a>(
    agentfs: &'a AgentFS,
    path: &'a str,
    resources: &'a mut Vec<JsonValue>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let dir_stats = match agentfs.fs.stat(path).await? {
            Some(s) => s,
            None => return Ok(()),
        };

        let entries = match agentfs.fs.readdir(dir_stats.ino).await? {
            Some(entries) => entries,
            None => return Ok(()),
        };

        for entry in entries {
            let full_path = if path == "/" {
                format!("/{}", entry)
            } else {
                format!("{}/{}", path, entry)
            };

            let stats = match agentfs.fs.stat(&full_path).await? {
                Some(s) => s,
                None => continue,
            };

            if stats.is_file() {
                resources.push(json!({
                    "uri": full_path,
                    "name": entry,
                    "description": format!("File at {}", full_path),
                    "mimeType": guess_mime_type(&full_path)
                }));
            } else if stats.is_directory() {
                // Recurse into subdirectory
                collect_file_resources(agentfs, &full_path, resources).await?;
            }
        }

        Ok(())
    })
}

/// Read a resource by path
async fn read_resource(agentfs: &AgentFS, path: &str) -> Result<Vec<u8>> {
    let normalized = normalize_path(path)?;

    let data = agentfs
        .fs
        .read_file(&normalized)
        .await
        .context("Failed to read file")?
        .ok_or_else(|| anyhow::anyhow!("File not found: {}", normalized))?;

    Ok(data)
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize)]
struct StatsResponse {
    ino: i64,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
}

impl From<Stats> for StatsResponse {
    fn from(stats: Stats) -> Self {
        Self {
            ino: stats.ino,
            mode: stats.mode,
            nlink: stats.nlink,
            uid: stats.uid,
            gid: stats.gid,
            size: stats.size,
            atime: stats.atime,
            mtime: stats.mtime,
            ctime: stats.ctime,
            is_file: stats.is_file(),
            is_directory: stats.is_directory(),
            is_symlink: stats.is_symlink(),
        }
    }
}

#[cfg(test)]
mod tests;
