use agentfs_core::{toolcalls::ToolCall, AgentFSOptions, ToolCalls};
use anyhow::{Context, Result as AnyhowResult};
use chrono::TimeZone;
use std::io::Write;
use std::str::FromStr;

use crate::cmd::init::{finalize_readonly, open_agentfs};

/// Output format for timeline display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "table" => Ok(OutputFormat::Table),
            "json" => Ok(OutputFormat::Json),
            _ => anyhow::bail!("Invalid format: {}", s),
        }
    }
}

/// Options for the timeline command
#[derive(Debug, Clone)]
pub struct TimelineOptions {
    pub limit: i64,
    pub filter: Option<String>,
    pub status: Option<String>,
    pub format: String,
}

/// Display agent action timeline from tool call audit log
pub async fn show_timeline(
    stdout: &mut impl Write,
    id_or_path: &str,
    options: &TimelineOptions,
) -> AnyhowResult<()> {
    let agent_options = AgentFSOptions::resolve(id_or_path)?;

    let agentfs = open_agentfs(agent_options)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, id_or_path))?;

    let result = show_timeline_opened(stdout, &agentfs, options).await;
    finalize_readonly(&agentfs).await;
    result
}

async fn show_timeline_opened(
    stdout: &mut impl Write,
    agentfs: &agentfs_core::AgentFS,
    options: &TimelineOptions,
) -> AnyhowResult<()> {
    let toolcalls = ToolCalls::from_pool(agentfs.get_pool())
        .await
        .context("Failed to create tool calls tracker")?;

    // Query tool calls
    let mut calls = toolcalls
        .recent(Some(options.limit))
        .await
        .context("Failed to query tool calls")?;

    // Apply filters
    if let Some(tool_name) = &options.filter {
        calls.retain(|call| call.name == *tool_name);
    }

    if let Some(status_filter) = &options.status {
        calls.retain(|call| call.status.to_string() == *status_filter);
    }

    // Format and display
    let output_format: OutputFormat = options.format.parse()?;
    match output_format {
        OutputFormat::Table => format_table(stdout, &calls)?,
        OutputFormat::Json => format_json(stdout, &calls)?,
    }

    Ok(())
}

/// Truncate a string to a maximum length, adding ellipsis if truncated
fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

/// Format a unix-seconds timestamp as YYYY-MM-DD HH:MM:SS. `tool_calls`
/// stores milliseconds, but `ToolCalls` normalizes to seconds on read, so
/// both this table column and the JSON form render second granularity.
fn format_timestamp(timestamp_secs: i64) -> String {
    chrono::Utc
        .timestamp_opt(timestamp_secs, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| format!("Invalid timestamp: {}", timestamp_secs))
}

/// Format tool calls in table format
fn format_table(stdout: &mut impl Write, calls: &[ToolCall]) -> AnyhowResult<()> {
    if calls.is_empty() {
        writeln!(stdout, "No tool calls found")?;
        return Ok(());
    }

    // Print header
    writeln!(
        stdout,
        "{:<4} {:<20} {:<10} {:>10} {:<20}",
        "ID", "TOOL", "STATUS", "DURATION", "STARTED"
    )?;

    // Print rows
    for call in calls {
        let tool_name = truncate_with_ellipsis(&call.name, 20);
        let status = call.status.to_string();
        let duration = call
            .duration_ms
            .map(|ms| format!("{}ms", ms))
            .unwrap_or_else(|| String::from("--"));
        let timestamp = format_timestamp(call.started_at);

        writeln!(
            stdout,
            "{:<4} {:<20} {:<10} {:>10} {:<20}",
            call.id, tool_name, status, duration, timestamp
        )?;
    }

    Ok(())
}

/// Format tool calls as JSON
fn format_json(stdout: &mut impl Write, calls: &[ToolCall]) -> AnyhowResult<()> {
    let json =
        serde_json::to_string_pretty(calls).context("Failed to serialize tool calls to JSON")?;
    writeln!(stdout, "{}", json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentfs_core::{AgentFS, AgentFSOptions};
    use tempfile::NamedTempFile;

    async fn create_test_agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()))
            .await
            .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    /// Create default TimelineOptions for testing
    fn default_options() -> TimelineOptions {
        TimelineOptions {
            limit: 100,
            filter: None,
            status: None,
            format: "table".to_string(),
        }
    }

    #[tokio::test]
    async fn test_timeline_empty() {
        let (_agentfs, path, _file) = create_test_agentfs().await;

        let mut buf = Vec::new();
        show_timeline(&mut buf, &path, &default_options())
            .await
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("No tool calls found"));
    }

    #[tokio::test]
    async fn test_timeline_table_format() {
        let (agentfs, path, _file) = create_test_agentfs().await;

        agentfs.tools.start("test_tool", None).await.unwrap();

        let mut buf = Vec::new();
        show_timeline(&mut buf, &path, &default_options())
            .await
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("ID"));
        assert!(output.contains("TOOL"));
        assert!(output.contains("STATUS"));
        assert!(output.contains("test_tool"));
    }

    #[tokio::test]
    async fn test_timeline_filter_by_name() {
        let (agentfs, path, _file) = create_test_agentfs().await;

        agentfs.tools.start("tool_a", None).await.unwrap();
        agentfs.tools.start("tool_b", None).await.unwrap();

        let mut buf = Vec::new();
        let options = TimelineOptions {
            filter: Some("tool_a".to_string()),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("tool_a"));
        assert!(!output.contains("tool_b"));

        // Test filter="nonexistent" returns no calls
        let mut buf = Vec::new();
        let options = TimelineOptions {
            filter: Some("nonexistent".to_string()),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("No tool calls found"));
    }

    #[tokio::test]
    async fn test_timeline_filter_by_status() {
        let (agentfs, path, _file) = create_test_agentfs().await;

        // Success call
        let success_id = agentfs.tools.start("test_tool", None).await.unwrap();
        agentfs
            .tools
            .success(success_id, Some(serde_json::json!({"success": true})))
            .await
            .unwrap();

        // Error call
        let error_id = agentfs.tools.start("test_tool", None).await.unwrap();
        agentfs.tools.error(error_id, "test error").await.unwrap();

        // Pending call
        agentfs.tools.start("test_tool", None).await.unwrap();

        // Test status="success" returns only successful calls
        let mut buf = Vec::new();
        let options = TimelineOptions {
            status: Some("success".to_string()),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("success"));
        assert!(!output.contains("error"));
        assert!(!output.contains("pending"));

        // Test status="error" returns only error calls
        let mut buf = Vec::new();
        let options = TimelineOptions {
            status: Some("error".to_string()),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("error"));
        assert!(!output.contains("success"));
        assert!(!output.contains("pending"));

        // Test status="pending" returns only pending calls
        let mut buf = Vec::new();
        let options = TimelineOptions {
            status: Some("pending".to_string()),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("pending"));
        assert!(!output.contains("success"));
        assert!(!output.contains("error"));
    }

    #[tokio::test]
    async fn test_timeline_limit() {
        let (agentfs, path, _file) = create_test_agentfs().await;

        // Record 5 tool calls
        for i in 0..5 {
            agentfs
                .tools
                .start(&format!("tool_{}", i), None)
                .await
                .unwrap();
        }

        // Test limit=2 returns exactly 2 calls
        let mut buf = Vec::new();
        let options = TimelineOptions {
            limit: 2,
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("tool_4"));
        assert!(output.contains("tool_3"));
        assert!(!output.contains("tool_2"));
        assert!(!output.contains("tool_1"));
        assert!(!output.contains("tool_0"));
    }

    #[tokio::test]
    async fn test_timeline_leaves_no_wal_sidecar() {
        let (agentfs, path, _file) = create_test_agentfs().await;
        agentfs.tools.start("test_tool", None).await.unwrap();
        drop(agentfs);

        let mut buf = Vec::new();
        show_timeline(&mut buf, &path, &default_options())
            .await
            .unwrap();

        assert!(
            !std::path::Path::new(&format!("{path}-wal")).exists(),
            "timeline reopen must not leave a WAL sidecar"
        );
        assert!(
            !std::path::Path::new(&format!("{path}-shm")).exists(),
            "timeline reopen must not leave an SHM sidecar"
        );
    }

    #[test]
    fn format_timestamp_renders_unix_seconds() {
        assert_eq!(format_timestamp(1_600_000_000), "2020-09-13 12:26:40");
    }

    #[tokio::test]
    async fn test_timeline_started_at_units_consistent() {
        let (agentfs, path, _file) = create_test_agentfs().await;
        // tool_calls stores milliseconds; both display forms must expose the
        // same second-granularity value.
        agentfs
            .tools
            .record(
                "clock",
                1_600_000_000_000,
                1_600_000_000_500,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let mut buf = Vec::new();
        show_timeline(&mut buf, &path, &default_options())
            .await
            .unwrap();
        let table = String::from_utf8(buf).unwrap();
        assert!(
            table.contains("2020-09-13 12:26:40"),
            "table must render millisecond-stored started_at as a date: {table}"
        );

        let mut buf = Vec::new();
        let options = TimelineOptions {
            format: "json".to_string(),
            ..default_options()
        };
        show_timeline(&mut buf, &path, &options).await.unwrap();
        let calls: Vec<serde_json::Value> = serde_json::from_slice(&buf).unwrap();
        assert_eq!(calls[0]["started_at"], 1_600_000_000);
        assert_eq!(calls[0]["completed_at"], 1_600_000_000);
        assert_eq!(calls[0]["duration_ms"], 500);
    }

    #[tokio::test]
    async fn test_timeline_truncate_long_names() {
        let (agentfs, path, _file) = create_test_agentfs().await;

        // Create a tool call with a very long name (>20 chars)
        agentfs
            .tools
            .start("very_long_tool_name_that_exceeds_twenty_characters", None)
            .await
            .unwrap();

        let mut buf = Vec::new();
        show_timeline(&mut buf, &path, &default_options())
            .await
            .unwrap();

        let output = String::from_utf8(buf).unwrap();
        // Should contain truncated version with ellipsis (20 chars total: 17 chars + "...")
        assert!(output.contains("very_long_tool_na..."));
        assert!(!output.contains("very_long_tool_name_that_exceeds_twenty_characters"));
    }
}
