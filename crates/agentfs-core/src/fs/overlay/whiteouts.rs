use super::*;
use turso::transaction::{Transaction, TransactionBehavior};

fn parent_path_for_whiteout(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }

    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

impl OverlayFS {
    /// Check if a path is whiteout (deleted from base).
    pub(super) fn is_whiteout(&self, path: &str) -> bool {
        let whiteouts = self.whiteouts.read();
        // Check path and all ancestors.
        let mut current = String::new();
        for component in path.split('/').filter(|s| !s.is_empty()) {
            current = format!("{current}/{component}");
            if whiteouts.contains(&current) {
                return true;
            }
        }
        false
    }

    /// Create a whiteout for a path.
    pub(super) async fn create_whiteout(&self, path: &str) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let parent_path = parent_path_for_whiteout(path);
        let (now, _) = current_timestamp()?;

        let result: Result<()> = async {
            conn.execute(
                "INSERT OR REPLACE INTO fs_whiteout (path, parent_path, created_at) VALUES (?, ?, ?)",
                (path, parent_path, now),
            )
            .await?;
            self.maybe_fail_whiteout_for_test()?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.whiteouts.write().insert(path.to_string());
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Remove a whiteout.
    pub(super) async fn remove_whiteout(&self, path: &str) -> Result<()> {
        if !self.whiteouts.read().contains(path) {
            return Ok(());
        }

        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<()> = async {
            conn.execute("DELETE FROM fs_whiteout WHERE path = ?", (path,))
                .await?;
            self.maybe_fail_whiteout_for_test()?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.whiteouts.write().remove(path);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Get child whiteouts for a directory.
    pub(super) fn get_child_whiteouts(&self, dir_path: &str) -> HashSet<String> {
        let whiteouts = self.whiteouts.read();
        let prefix = if dir_path == "/" {
            "/".to_string()
        } else {
            format!("{dir_path}/")
        };
        whiteouts
            .iter()
            .filter_map(|p| {
                if dir_path == "/" {
                    let trimmed = p.trim_start_matches('/');
                    if !trimmed.contains('/') {
                        Some(trimmed.to_string())
                    } else {
                        None
                    }
                } else if p.starts_with(&prefix) {
                    let rest = &p[prefix.len()..];
                    if !rest.contains('/') {
                        Some(rest.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn fail_next_whiteout_for_test(&self, reason: &str) {
        *self.whiteout_fault.lock() = Some(reason.to_string());
    }

    fn maybe_fail_whiteout_for_test(&self) -> Result<()> {
        #[cfg(test)]
        {
            if let Some(reason) = self.whiteout_fault.lock().take() {
                return Err(Error::Internal(reason));
            }
        }
        Ok(())
    }
}
