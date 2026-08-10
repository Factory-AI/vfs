// Everything here has its only production consumer in the Linux mount owner
// (`CtlServer`), so items shared with the tests carry `any(linux, test)` and
// macOS lib builds compile this module down to nothing rather than failing
// -D dead-code.

#[cfg(any(target_os = "linux", test))]
use {
    super::{chunk_key, RemoteStore},
    anyhow::{Context, Result},
    bytes::Bytes,
    futures_util::{stream, StreamExt},
    std::collections::HashSet,
    vfs_core::Vfs,
};

#[cfg(target_os = "linux")]
use {
    super::RemoteConfig,
    std::{sync::Arc, time::Duration},
};

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StreamReport {
    pub(crate) uploaded: usize,
    pub(crate) bytes: u64,
}

/// Upload every locally present chunk not already known to exist remotely.
#[cfg(any(target_os = "linux", test))]
pub(crate) async fn stream_once(
    vfs: &Vfs,
    store: &RemoteStore,
    uploaded: &mut HashSet<[u8; 32]>,
    concurrency: usize,
) -> Result<StreamReport> {
    let pending: Vec<_> = vfs
        .chunk_digests()
        .await?
        .into_iter()
        .filter(|digest| !uploaded.contains(digest))
        .collect();
    let mut uploads = stream::iter(pending)
        .map(|digest| async move {
            let Some(data) = vfs.chunk_data(&digest).await? else {
                return Ok((digest, None));
            };
            let bytes = data.len() as u64;
            store
                .put(&chunk_key(&digest), Bytes::from(data))
                .await
                .with_context(|| {
                    format!(
                        "Failed to stream remote chunk {}",
                        blake3::Hash::from_bytes(digest).to_hex()
                    )
                })?;
            Ok((digest, Some(bytes)))
        })
        .buffer_unordered(concurrency.max(1));

    let mut report = StreamReport::default();
    let mut first_error = None;
    while let Some(result) = uploads.next().await {
        match result {
            Ok((digest, Some(bytes))) => {
                uploaded.insert(digest);
                report.uploaded += 1;
                report.bytes += bytes;
            }
            Ok((_digest, None)) => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(report),
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_chunk_key(key: &str) -> Option<[u8; 32]> {
    let encoded = key.strip_prefix("chunks/")?;
    let bytes = hex::decode(encoded).ok()?;
    <[u8; 32]>::try_from(bytes).ok()
}

#[cfg(target_os = "linux")]
pub(crate) struct RemoteStreamer {
    task: tokio::task::JoinHandle<()>,
}

#[cfg(target_os = "linux")]
impl RemoteStreamer {
    pub(crate) fn spawn(vfs: Arc<Vfs>, config: RemoteConfig) -> Result<Self> {
        let store = RemoteStore::new(&config.url)?;
        let task = tokio::spawn(run_loop(
            vfs,
            store,
            config.concurrency,
            config.stream_interval_ms,
        ));
        Ok(Self { task })
    }

    pub(crate) fn shutdown(self) {
        self.task.abort();
    }
}

#[cfg(target_os = "linux")]
async fn run_loop(vfs: Arc<Vfs>, store: RemoteStore, concurrency: usize, interval_ms: u64) {
    let mut uploaded = match store.list("chunks/").await {
        Ok(keys) => keys
            .into_iter()
            .filter_map(|key| parse_chunk_key(&key))
            .collect(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to seed remote chunk streamer; existing chunks may be re-uploaded"
            );
            HashSet::new()
        }
    };
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));

    loop {
        interval.tick().await;
        match stream_once(&vfs, &store, &mut uploaded, concurrency).await {
            Ok(report) if report.uploaded > 0 => {
                tracing::debug!(
                    chunks = report.uploaded,
                    bytes = report.bytes,
                    "streamed remote chunks"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = %error, "remote chunk streaming pass failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use url::Url;
    use vfs_core::VfsOptions;

    async fn open_vfs(dir: &Path) -> Vfs {
        Vfs::open(VfsOptions::with_path(
            dir.join("streamer.db").to_string_lossy(),
        ))
        .await
        .unwrap()
    }

    async fn write_chunked_file(vfs: &Vfs, path: &str, seed: u8) {
        let content: Vec<u8> = (0..100_000)
            .map(|index| seed.wrapping_add((index % 251) as u8))
            .collect();
        let (_, file) = vfs.fs.create_file(path, 0o100644, 0, 0).await.unwrap();
        file.pwrite(0, &content).await.unwrap();
        file.fsync().await.unwrap();
    }

    fn local_store(root: &Path) -> RemoteStore {
        let url = Url::from_directory_path(root).unwrap().to_string();
        RemoteStore::new(&url).unwrap()
    }

    async fn assert_remote_chunks_match_keys(store: &RemoteStore) {
        for key in store.list("chunks/").await.unwrap() {
            let data = store.get(&key).await.unwrap();
            assert_eq!(
                key.strip_prefix("chunks/").unwrap(),
                blake3::hash(&data).to_hex().as_str()
            );
        }
    }

    #[tokio::test]
    async fn stream_once_uploads_new_chunks_only() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let vfs = open_vfs(local.path()).await;
        write_chunked_file(&vfs, "/first.bin", 3).await;
        let store = local_store(remote.path());
        let mut uploaded = HashSet::new();

        let first_digests = vfs.chunk_digests().await.unwrap();
        let first_bytes = futures_util::future::join_all(
            first_digests.iter().map(|digest| vfs.chunk_data(digest)),
        )
        .await
        .into_iter()
        .map(|result| result.unwrap().unwrap().len() as u64)
        .sum::<u64>();
        let first = stream_once(&vfs, &store, &mut uploaded, 2).await.unwrap();
        assert_eq!(first.uploaded, first_digests.len());
        assert_eq!(first.bytes, first_bytes);
        assert_eq!(uploaded.len(), first_digests.len());
        assert_remote_chunks_match_keys(&store).await;

        assert_eq!(
            stream_once(&vfs, &store, &mut uploaded, 2).await.unwrap(),
            StreamReport::default()
        );

        let before = uploaded.clone();
        write_chunked_file(&vfs, "/second.bin", 117).await;
        let expected_delta = vfs
            .chunk_digests()
            .await
            .unwrap()
            .into_iter()
            .filter(|digest| !before.contains(digest))
            .count();
        let third = stream_once(&vfs, &store, &mut uploaded, 2).await.unwrap();
        assert_eq!(third.uploaded, expected_delta);
        assert_eq!(uploaded.len(), before.len() + expected_delta);
        assert_remote_chunks_match_keys(&store).await;
    }

    #[tokio::test]
    async fn remote_list_seeds_existing_chunks() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let vfs = open_vfs(local.path()).await;
        write_chunked_file(&vfs, "/seeded.bin", 29).await;
        let store = local_store(remote.path());
        let digests = vfs.chunk_digests().await.unwrap();
        let existing = digests[0];
        let existing_data = Bytes::from_static(b"already remote");
        store
            .put(&chunk_key(&existing), existing_data.clone())
            .await
            .unwrap();
        store
            .put("chunks/not-a-digest", Bytes::from_static(b"ignored"))
            .await
            .unwrap();

        let mut uploaded: HashSet<_> = store
            .list("chunks/")
            .await
            .unwrap()
            .into_iter()
            .filter_map(|key| parse_chunk_key(&key))
            .collect();
        assert_eq!(uploaded, HashSet::from([existing]));

        let report = stream_once(&vfs, &store, &mut uploaded, 3).await.unwrap();
        assert_eq!(report.uploaded, digests.len() - 1);
        assert_eq!(uploaded.len(), digests.len());
        assert_eq!(
            store.get(&chunk_key(&existing)).await.unwrap(),
            existing_data
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_put_is_retried_after_remote_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let vfs = open_vfs(local.path()).await;
        write_chunked_file(&vfs, "/retry.bin", 61).await;
        let digest = vfs.chunk_digests().await.unwrap()[0];
        let store = local_store(remote.path());
        std::fs::set_permissions(remote.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut uploaded = HashSet::new();

        let error = stream_once(&vfs, &store, &mut uploaded, 1)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("Failed to put remote object"));
        assert!(!uploaded.contains(&digest));

        std::fs::set_permissions(remote.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let report = stream_once(&vfs, &store, &mut uploaded, 1).await.unwrap();
        assert!(report.uploaded > 0);
        assert!(uploaded.contains(&digest));
    }
}
