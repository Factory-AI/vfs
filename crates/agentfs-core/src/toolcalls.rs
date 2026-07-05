use crate::error::{Error, Result};
use crate::pool::{ConnectionPool, DatabaseType};
use crate::schema;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};
use turso::{Builder, Value};

/// Status of a tool call
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Pending,
    Success,
    Error,
}

impl fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolCallStatus::Pending => write!(f, "pending"),
            ToolCallStatus::Success => write!(f, "success"),
            ToolCallStatus::Error => write!(f, "error"),
        }
    }
}

impl TryFrom<&str> for ToolCallStatus {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(ToolCallStatus::Pending),
            "success" => Ok(ToolCallStatus::Success),
            "error" => Ok(ToolCallStatus::Error),
            other => Err(Error::Internal(format!(
                "invalid tool call status: {other}"
            ))),
        }
    }
}

/// A tool call record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: i64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: ToolCallStatus,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Statistics for a specific tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStats {
    pub name: String,
    pub total_calls: i64,
    pub successful: i64,
    pub failed: i64,
    pub avg_duration_ms: f64,
}

/// Tool calls tracker backed by SQLite
#[derive(Clone)]
pub struct ToolCalls {
    pool: ConnectionPool,
}

impl ToolCalls {
    /// Create a new tool calls tracker
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let options = if db_path == ":memory:" {
            crate::fs::agentfs::memory_connection_pool_options()
        } else {
            crate::fs::agentfs::file_backed_connection_pool_options()
        };
        let pool = ConnectionPool::with_options(DatabaseType::Local(db), options);
        let tc = Self { pool };
        tc.initialize().await?;
        Ok(tc)
    }

    /// Create a tool calls tracker from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        let tc = Self { pool };
        tc.initialize().await?;
        Ok(tc)
    }

    /// Initialize the database schema
    async fn initialize(&self) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        schema::require_current(&conn).await
    }

    /// Start a new tool call and mark it as pending
    /// Returns the ID of the created tool call record
    pub async fn start(&self, name: &str, parameters: Option<serde_json::Value>) -> Result<i64> {
        let conn = self.pool.get_connection().await?;
        let serialized_params = parameters.map(|p| serde_json::to_string(&p)).transpose()?;
        let started_at = unix_timestamp_ms()?;

        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO tool_calls (name, parameters, status, started_at)
                VALUES (?, ?, 'pending', ?) RETURNING id",
            )
            .await?;
        let row = stmt
            .query_row((name, serialized_params.as_deref().unwrap_or(""), started_at))
            .await?;

        let id = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| Error::Internal("failed to get tool call ID".to_string()))?;
        Ok(id)
    }

    /// Mark a tool call as successful
    pub async fn success(&self, id: i64, result: Option<serde_json::Value>) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let serialized_result = result.map(|r| serde_json::to_string(&r)).transpose()?;
        let completed_at = unix_timestamp_ms()?;

        // Get the started_at time to calculate duration
        let mut stmt = conn
            .prepare_cached("SELECT started_at FROM tool_calls WHERE id = ?")
            .await?;
        let mut rows = stmt.query((id,)).await?;

        let started_at = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("invalid started_at value".to_string()))?
        } else {
            return Err(Error::ToolCallNotFound);
        };

        let duration_ms = duration_ms_between(started_at, completed_at);

        let mut stmt = conn
            .prepare_cached(
                "UPDATE tool_calls
            SET result = ?, status = 'success', completed_at = ?, duration_ms = ?
            WHERE id = ?",
            )
            .await?;
        stmt.execute((
            serialized_result.as_deref().unwrap_or(""),
            completed_at,
            duration_ms,
            id,
        ))
        .await?;

        Ok(())
    }

    /// Record a completed tool call (spec-compliant insert-only method)
    /// Either result or error should be provided, not both
    /// Returns the ID of the created tool call record
    pub async fn record(
        &self,
        name: &str,
        started_at: i64,
        completed_at: i64,
        parameters: Option<serde_json::Value>,
        result: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> Result<i64> {
        let conn = self.pool.get_connection().await?;
        let serialized_params = parameters.map(|p| serde_json::to_string(&p)).transpose()?;
        let serialized_result = result.map(|r| serde_json::to_string(&r)).transpose()?;
        let duration_ms = duration_ms_between(started_at, completed_at);
        let status = if error.is_some() { "error" } else { "success" };

        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO tool_calls (name, parameters, result, error, status, started_at, completed_at, duration_ms)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
            )
            .await?;

        let row = stmt
            .query_row((
                name,
                serialized_params.as_deref().unwrap_or(""),
                serialized_result.as_deref().unwrap_or(""),
                error.unwrap_or(""),
                status,
                started_at,
                completed_at,
                duration_ms,
            ))
            .await?;
        let id = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| Error::Internal("failed to get tool call ID".to_string()))?;
        Ok(id)
    }

    /// Mark a tool call as failed
    pub async fn error(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let completed_at = unix_timestamp_ms()?;

        // Get the started_at time to calculate duration
        let mut stmt = conn
            .prepare_cached("SELECT started_at FROM tool_calls WHERE id = ?")
            .await?;
        let mut rows = stmt.query((id,)).await?;

        let started_at = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("invalid started_at value".to_string()))?
        } else {
            return Err(Error::ToolCallNotFound);
        };

        let duration_ms = duration_ms_between(started_at, completed_at);

        let mut stmt = conn
            .prepare_cached(
                "UPDATE tool_calls
            SET error = ?, status = 'error', completed_at = ?, duration_ms = ?
            WHERE id = ?",
            )
            .await?;
        stmt.execute((error, completed_at, duration_ms, id)).await?;

        Ok(())
    }

    /// Get a tool call by ID
    pub async fn get(&self, id: i64) -> Result<Option<ToolCall>> {
        let conn = self.pool.get_connection().await?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, name, parameters, result, error, status, started_at, completed_at, duration_ms
                FROM tool_calls WHERE id = ?",
            )
            .await?;
        let mut rows = stmt.query((id,)).await?;

        if let Some(row) = rows.next().await? {
            Ok(Some(Self::row_to_tool_call(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Get recent tool calls with optional limit
    pub async fn recent(&self, limit: Option<i64>) -> Result<Vec<ToolCall>> {
        let conn = self.pool.get_connection().await?;
        let limit = limit.unwrap_or(100);
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, name, parameters, result, error, status, started_at, completed_at, duration_ms
                FROM tool_calls
                ORDER BY started_at DESC
                LIMIT ?",
            )
            .await?;
        let mut rows = stmt.query((limit,)).await?;

        let mut calls = Vec::new();
        while let Some(row) = rows.next().await? {
            calls.push(Self::row_to_tool_call(&row)?);
        }

        Ok(calls)
    }

    /// Get statistics for a specific tool
    pub async fn stats_for(&self, name: &str) -> Result<Option<ToolCallStats>> {
        let conn = self.pool.get_connection().await?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT
                    name,
                    COUNT(*) as total_calls,
                    SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
                    AVG(CASE WHEN duration_ms IS NOT NULL THEN duration_ms ELSE 0 END) as avg_duration_ms
                FROM tool_calls
                WHERE name = ?
                GROUP BY name",
            )
            .await?;
        let mut rows = stmt.query((name,)).await?;

        if let Some(row) = rows.next().await? {
            Ok(Some(Self::row_to_stats(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Get statistics for all tools
    pub async fn stats(&self) -> Result<Vec<ToolCallStats>> {
        let conn = self.pool.get_connection().await?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT
                    name,
                    COUNT(*) as total_calls,
                    SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
                    AVG(CASE WHEN duration_ms IS NOT NULL THEN duration_ms ELSE 0 END) as avg_duration_ms
                FROM tool_calls
                GROUP BY name
                ORDER BY total_calls DESC",
            )
            .await?;
        let mut rows = stmt.query(()).await?;

        let mut stats = Vec::new();
        while let Some(row) = rows.next().await? {
            stats.push(Self::row_to_stats(&row)?);
        }

        Ok(stats)
    }

    fn row_to_tool_call(row: &turso::Row) -> Result<ToolCall> {
        let id = required_i64(row, 0, "id")?;
        let name = required_text(row, 1, "name")?;
        let parameters = optional_json(row, 2, "parameters")?;
        let result = optional_json(row, 3, "result")?;
        let error = optional_text(row, 4, "error")?;
        let status = ToolCallStatus::try_from(required_text(row, 5, "status")?.as_str())?;
        let started_at = normalize_timestamp_for_output(required_i64(row, 6, "started_at")?);
        let completed_at =
            optional_i64(row, 7, "completed_at")?.map(normalize_timestamp_for_output);
        let duration_ms = optional_i64(row, 8, "duration_ms")?;

        Ok(ToolCall {
            id,
            name,
            parameters,
            result,
            error,
            status,
            started_at,
            completed_at,
            duration_ms,
        })
    }

    fn row_to_stats(row: &turso::Row) -> Result<ToolCallStats> {
        let name = required_text(row, 0, "name")?;
        let total_calls = required_i64(row, 1, "total_calls")?;
        let successful = required_i64(row, 2, "successful")?;
        let failed = required_i64(row, 3, "failed")?;
        let avg_duration_ms = required_f64(row, 4, "avg_duration_ms")?;

        Ok(ToolCallStats {
            name,
            total_calls,
            successful,
            failed,
            avg_duration_ms,
        })
    }
}

fn unix_timestamp_ms() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::Internal("current timestamp exceeds i64 milliseconds".to_string()))
}

fn duration_ms_between(started_at: i64, completed_at: i64) -> i64 {
    timestamp_ms(completed_at).saturating_sub(timestamp_ms(started_at))
}

fn timestamp_ms(timestamp: i64) -> i64 {
    if timestamp.abs() >= 1_000_000_000_000 {
        timestamp
    } else {
        timestamp.saturating_mul(1000)
    }
}

fn normalize_timestamp_for_output(timestamp: i64) -> i64 {
    if timestamp.abs() >= 1_000_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    }
}

fn required_i64(row: &turso::Row, idx: usize, column: &str) -> Result<i64> {
    row.get_value(idx)
        .ok()
        .and_then(|v| v.as_integer().copied())
        .ok_or_else(|| Error::Internal(format!("invalid tool_calls.{column}: expected integer")))
}

fn optional_i64(row: &turso::Row, idx: usize, column: &str) -> Result<Option<i64>> {
    match row.get_value(idx).ok() {
        Some(Value::Null) | None => Ok(None),
        Some(value) => value.as_integer().copied().map(Some).ok_or_else(|| {
            Error::Internal(format!("invalid tool_calls.{column}: expected integer"))
        }),
    }
}

fn required_text(row: &turso::Row, idx: usize, column: &str) -> Result<String> {
    optional_text(row, idx, column)?
        .ok_or_else(|| Error::Internal(format!("invalid tool_calls.{column}: expected text")))
}

fn optional_text(row: &turso::Row, idx: usize, column: &str) -> Result<Option<String>> {
    match row.get_value(idx).ok() {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Text(s)) if s.is_empty() => Ok(None),
        Some(Value::Text(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::Internal(format!(
            "invalid tool_calls.{column}: expected text"
        ))),
    }
}

fn optional_json(row: &turso::Row, idx: usize, column: &str) -> Result<Option<serde_json::Value>> {
    optional_text(row, idx, column)?
        .map(|s| serde_json::from_str(s.as_str()).map_err(Error::from))
        .transpose()
}

fn required_f64(row: &turso::Row, idx: usize, column: &str) -> Result<f64> {
    match row.get_value(idx).ok() {
        Some(Value::Real(value)) => Ok(value),
        Some(Value::Integer(value)) => Ok(value as f64),
        _ => Err(Error::Internal(format!(
            "invalid tool_calls.{column}: expected number"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn start_success_records_millisecond_duration() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("tools.db");
        let tools = ToolCalls::new(db_path.to_str().unwrap()).await?;

        let id = tools.start("fast", None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        tools.success(id, None).await?;

        let call = tools.get(id).await?.expect("tool call should exist");
        let duration_ms = call.duration_ms.expect("completed call has duration");
        assert!(
            (20..2000).contains(&duration_ms),
            "duration_ms should be millisecond-scaled, got {duration_ms}"
        );
        assert!(
            call.completed_at.unwrap() >= call.started_at,
            "decoded timestamps should remain unix seconds for consumers"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_toolcall_json_propagates() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("tools.db");
        let tools = ToolCalls::new(db_path.to_str().unwrap()).await?;
        let id = tools.start("bad-json", None).await?;

        let conn = tools.pool.get_connection().await?;
        conn.execute(
            "UPDATE tool_calls SET parameters = ? WHERE id = ?",
            ("{not-json", id),
        )
        .await?;
        drop(conn);

        let error = tools
            .get(id)
            .await
            .expect_err("invalid JSON should propagate");
        assert!(
            matches!(error, Error::Json(_)),
            "expected JSON decode error, got {error:?}"
        );
        Ok(())
    }
}
