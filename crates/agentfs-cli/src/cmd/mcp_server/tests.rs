use super::*;
use agentfs_core::fs::S_IFREG;
use agentfs_core::{error::Error as SdkError, FsError, ToolCallStatus};

const ROOT_INO: i64 = 1;

/// Unit tests run against an ephemeral database (Held source) so state and
/// audit rows are shared across requests within one server. The PerRequest
/// reopen semantics are inherently cross-process and are covered by the
/// stdio shell suite (tests/test-mcp-server.sh).
async fn create_test_server() -> Result<(McpServer, Arc<AgentFS>)> {
    create_filtered_test_server(None).await
}

async fn create_filtered_test_server(
    tools_filter: Option<Vec<String>>,
) -> Result<(McpServer, Arc<AgentFS>)> {
    let server = McpServer::new(AgentFSOptions::ephemeral(), tools_filter).await?;
    let agentfs = server.agentfs().await?;
    Ok((server, agentfs))
}

async fn call_tool(server: &McpServer, id: i64, name: &str, arguments: JsonValue) -> JsonValue {
    server
        .handle_request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .await
        .expect("tools/call requests must produce a response")
}

fn advertised_tool_names(server: &McpServer) -> Vec<String> {
    server
        .get_tool_definitions()
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(|name| name.as_str())
                .expect("every tool definition has a name")
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn tools_list_equals_dispatch_surface() -> Result<()> {
    let (server, agentfs) = create_test_server().await?;

    assert_eq!(
        advertised_tool_names(&server),
        ALL_TOOLS.to_vec(),
        "tools/list must advertise exactly the canonical tool surface"
    );

    // Every advertised tool must dispatch with minimal valid arguments.
    agentfs.fs.mkdir("/dir", 0, 0).await?;
    let minimal_args = |name: &str| -> JsonValue {
        match name {
            "read_file" => json!({"path": "/seed.txt"}),
            "write_file" => json!({"path": "/seed.txt", "content": "seed"}),
            "readdir" => json!({"path": "/"}),
            "mkdir" => json!({"path": "/made"}),
            "remove" => json!({"path": "/made"}),
            "rename" => json!({"from": "/seed.txt", "to": "/renamed.txt"}),
            "stat" => json!({"path": "/renamed.txt"}),
            "access" => json!({"path": "/renamed.txt"}),
            "kv_get" => json!({"key": "k"}),
            "kv_set" => json!({"key": "k", "value": {"nested": true}}),
            "kv_delete" => json!({"key": "k"}),
            "kv_list" => json!({}),
            other => panic!("no minimal arguments defined for tool {other}"),
        }
    };
    // Dispatch order satisfies data dependencies (write before read/rename).
    let order = [
        "write_file",
        "read_file",
        "readdir",
        "mkdir",
        "remove",
        "rename",
        "stat",
        "access",
        "kv_set",
        "kv_get",
        "kv_list",
        "kv_delete",
    ];
    assert_eq!(
        {
            let mut sorted = order.to_vec();
            sorted.sort_unstable();
            sorted
        },
        {
            let mut sorted = ALL_TOOLS.to_vec();
            sorted.sort_unstable();
            sorted
        },
        "dispatch-order fixture must cover the canonical tool surface"
    );

    for (idx, name) in order.iter().enumerate() {
        let response = call_tool(&server, idx as i64 + 1, name, minimal_args(name)).await;
        assert!(
            response.get("error").is_none(),
            "advertised tool {name} failed to dispatch: {response}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn kv_list_dispatches_and_honors_prefix() -> Result<()> {
    let (_server, agentfs) = create_test_server().await?;
    agentfs.kv.set("app/one", &json!(1)).await?;
    agentfs.kv.set("app/two", &json!(2)).await?;
    agentfs.kv.set("other", &json!(3)).await?;

    let all = handle_kv_list(&agentfs, KvListParams { prefix: None }).await?;
    let all: Vec<String> = serde_json::from_str(&all)?;
    assert_eq!(all, vec!["app/one", "app/two", "other"]);

    let filtered = handle_kv_list(
        &agentfs,
        KvListParams {
            prefix: Some("app/".to_string()),
        },
    )
    .await?;
    let filtered: Vec<String> = serde_json::from_str(&filtered)?;
    assert_eq!(filtered, vec!["app/one", "app/two"]);
    Ok(())
}

#[tokio::test]
async fn tool_calls_are_audited_with_status_and_timing() -> Result<()> {
    let (server, agentfs) = create_test_server().await?;

    let ok = call_tool(
        &server,
        1,
        "write_file",
        json!({"path": "/audit.txt", "content": "hello"}),
    )
    .await;
    assert!(ok.get("error").is_none(), "write_file failed: {ok}");

    let failed = call_tool(&server, 2, "read_file", json!({"path": "/missing.txt"})).await;
    assert!(
        failed.get("error").is_some(),
        "read_file on a missing path must fail"
    );

    let calls = agentfs.tools.recent(None).await?;
    assert_eq!(calls.len(), 2, "each tools/call must write one audit row");

    // recent() orders by started_at, which can tie at millisecond
    // resolution; look rows up by name instead.
    let read_call = calls
        .iter()
        .find(|call| call.name == "read_file")
        .expect("read_file audit row");
    assert_eq!(read_call.status, ToolCallStatus::Error);
    assert!(
        read_call
            .error
            .as_deref()
            .unwrap_or("")
            .contains("/missing.txt"),
        "audit row should record the tool error, got {:?}",
        read_call.error
    );

    let write_call = calls
        .iter()
        .find(|call| call.name == "write_file")
        .expect("write_file audit row");
    assert_eq!(write_call.status, ToolCallStatus::Success);
    assert_eq!(
        write_call.parameters,
        Some(json!({"path": "/audit.txt", "content": "hello"})),
        "audit row should record the tool arguments"
    );
    assert_eq!(
        write_call.result,
        Some(json!("Wrote 5 bytes to /audit.txt")),
        "audit row should record the tool result"
    );
    assert!(write_call.duration_ms.is_some());
    assert!(write_call.completed_at.is_some());
    Ok(())
}

#[tokio::test]
async fn write_file_preserves_existing_mode_and_creates_with_default() -> Result<()> {
    let (server, agentfs) = create_test_server().await?;

    let (created, file) = agentfs
        .fs
        .create_file("/mode.txt", S_IFREG | 0o755, 0, 0)
        .await?;
    file.pwrite(0, b"original content").await?;
    file.drain_writes().await?;
    assert_eq!(created.mode, S_IFREG | 0o755);

    let response = call_tool(
        &server,
        1,
        "write_file",
        json!({"path": "/mode.txt", "content": "overwritten"}),
    )
    .await;
    assert!(response.get("error").is_none(), "write failed: {response}");

    let stats = agentfs.fs.stat("/mode.txt").await?.unwrap();
    assert_eq!(
        stats.mode,
        S_IFREG | 0o755,
        "overwriting an existing file must not change its mode"
    );
    assert_eq!(stats.size, "overwritten".len() as i64);
    assert_eq!(
        agentfs.fs.read_file("/mode.txt").await?.unwrap(),
        b"overwritten".to_vec()
    );

    let response = call_tool(
        &server,
        2,
        "write_file",
        json!({"path": "/fresh.txt", "content": "new"}),
    )
    .await;
    assert!(response.get("error").is_none(), "write failed: {response}");
    let stats = agentfs.fs.stat("/fresh.txt").await?.unwrap();
    assert_eq!(
        stats.mode, DEFAULT_FILE_MODE,
        "new files must be created with the documented default mode 0644"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_tool_is_rejected_with_invalid_params_and_audited() -> Result<()> {
    let (server, agentfs) = create_test_server().await?;

    let response = call_tool(&server, 1, "copy_file", json!({})).await;
    let error = response
        .get("error")
        .expect("unknown tool must produce a JSON-RPC error");
    assert_eq!(
        error.get("code").and_then(|c| c.as_i64()),
        Some(INVALID_PARAMS)
    );
    assert!(
        error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .contains("copy_file"),
        "error should name the unknown tool: {error}"
    );

    let calls = agentfs.tools.recent(None).await?;
    assert_eq!(calls.len(), 1, "rejected calls are audited too");
    assert_eq!(calls[0].name, "copy_file");
    assert_eq!(calls[0].status, ToolCallStatus::Error);
    Ok(())
}

#[tokio::test]
async fn notifications_are_accepted_without_response() -> Result<()> {
    let (server, _agentfs) = create_test_server().await?;

    // The standard post-initialize notification from strict clients.
    assert_eq!(
        server
            .handle_request(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await,
        None,
        "notifications/initialized must be accepted without a response"
    );
    // Legacy spelling.
    assert_eq!(
        server
            .handle_request(json!({"jsonrpc": "2.0", "method": "initialized"}))
            .await,
        None
    );
    // Any request without an id is a notification and never gets a response,
    // even when it would error.
    assert_eq!(
        server
            .handle_request(json!({"jsonrpc": "2.0", "method": "bogus/method"}))
            .await,
        None
    );
    Ok(())
}

#[tokio::test]
async fn unknown_method_with_id_returns_method_not_found() -> Result<()> {
    let (server, _agentfs) = create_test_server().await?;

    let response = server
        .handle_request(json!({"jsonrpc": "2.0", "id": 7, "method": "bogus/method"}))
        .await
        .expect("requests with an id must produce a response");
    let error = response.get("error").expect("unknown method must error");
    assert_eq!(
        error.get("code").and_then(|c| c.as_i64()),
        Some(METHOD_NOT_FOUND)
    );
    assert_eq!(response.get("id").and_then(|id| id.as_i64()), Some(7));
    Ok(())
}

#[tokio::test]
async fn tools_filter_limits_both_listing_and_dispatch() -> Result<()> {
    let (server, agentfs) =
        create_filtered_test_server(Some(vec!["read_file".to_string(), "kv_list".to_string()]))
            .await?;

    assert_eq!(
        advertised_tool_names(&server),
        vec!["read_file".to_string(), "kv_list".to_string()],
        "tools/list must contain exactly the filtered tools"
    );

    let response = call_tool(
        &server,
        1,
        "write_file",
        json!({"path": "/blocked.txt", "content": "nope"}),
    )
    .await;
    let error = response
        .get("error")
        .expect("disabled tools must be rejected at call time");
    assert_eq!(
        error.get("code").and_then(|c| c.as_i64()),
        Some(INVALID_PARAMS)
    );
    assert!(
        agentfs.fs.stat("/blocked.txt").await?.is_none(),
        "disabled tool must not have side effects"
    );

    let calls = agentfs.tools.recent(None).await?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, ToolCallStatus::Error);
    Ok(())
}

#[tokio::test]
async fn unknown_tools_filter_is_rejected_at_startup() -> Result<()> {
    let error = match McpServer::new(
        AgentFSOptions::ephemeral(),
        Some(vec!["copy_file".to_string()]),
    )
    .await
    {
        Ok(_) => panic!("phantom tool names must be rejected at startup"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("copy_file") && message.contains("kv_list"),
        "startup error should name the unknown tool and list the available ones: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_rename_directory_into_own_subtree_returns_error_and_preserves_namespace() -> Result<()>
{
    let (server, agentfs) = create_test_server().await?;
    agentfs.fs.mkdir("/parent", 0, 0).await?;
    agentfs.fs.mkdir("/parent/child", 0, 0).await?;

    let parent_ino = agentfs.fs.stat("/parent").await?.unwrap().ino;
    let child_ino = agentfs.fs.stat("/parent/child").await?.unwrap().ino;
    let root_before = agentfs.fs.readdir(ROOT_INO).await?.unwrap();
    let parent_before = agentfs.fs.readdir(parent_ino).await?.unwrap();
    let child_before = agentfs.fs.readdir(child_ino).await?.unwrap();

    let direct_error = handle_rename(
        &agentfs,
        RenameParams {
            from: "/parent".to_string(),
            to: "/parent/child/parent".to_string(),
        },
    )
    .await
    .expect_err("cycle rename should fail");
    assert!(
        direct_error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<SdkError>(),
                Some(SdkError::Fs(FsError::InvalidRename))
            )
        }),
        "expected InvalidRename in error chain, got {direct_error:#}"
    );

    let response = server
        .handle_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "rename",
                "arguments": {
                    "from": "/parent",
                    "to": "/parent/child/parent"
                }
            }
        }))
        .await
        .expect("tools/call requests must produce a response");
    assert!(
        response.get("error").is_some(),
        "cycle rename should return a JSON-RPC error response: {response}"
    );

    assert_eq!(agentfs.fs.readdir(ROOT_INO).await?.unwrap(), root_before);
    assert_eq!(
        agentfs.fs.readdir(parent_ino).await?.unwrap(),
        parent_before
    );
    assert_eq!(agentfs.fs.readdir(child_ino).await?.unwrap(), child_before);
    assert!(agentfs.fs.stat("/parent").await?.is_some());
    assert!(agentfs.fs.stat("/parent/child").await?.is_some());
    assert!(agentfs.fs.stat("/parent/child/parent").await?.is_none());
    Ok(())
}
