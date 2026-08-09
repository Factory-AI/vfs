use std::path::Path as FsPath;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use url::Url;

/// Object-store client rooted at the path prefix in its configured URL.
pub struct RemoteStore {
    inner: Arc<dyn ObjectStore>,
    base: Path,
}

impl RemoteStore {
    pub fn new(remote_url: &str) -> Result<Self> {
        if remote_url.starts_with("file:") && !remote_url.starts_with("file:///") {
            bail!("file remote URL must contain an absolute path: {remote_url}");
        }
        let url =
            Url::parse(remote_url).with_context(|| format!("Invalid remote URL {remote_url:?}"))?;
        if url.query().is_some() || url.fragment().is_some() {
            bail!("remote URL must not contain a query string or fragment: {remote_url}");
        }

        match url.scheme() {
            "s3" => Self::s3(&url),
            "file" => Self::local(&url),
            scheme => bail!("unsupported remote URL scheme {scheme:?}; expected s3:// or file://"),
        }
    }

    /// Store bytes at a key relative to the configured remote prefix.
    pub async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        let location = self.location(key)?;
        self.inner
            .put(&location, bytes.into())
            .await
            .with_context(|| format!("Failed to put remote object {key:?}"))?;
        Ok(())
    }

    /// Read bytes from a key relative to the configured remote prefix.
    pub async fn get(&self, key: &str) -> Result<Bytes> {
        let location = self.location(key)?;
        self.inner
            .get(&location)
            .await
            .with_context(|| format!("Failed to get remote object {key:?}"))?
            .bytes()
            .await
            .with_context(|| format!("Failed to read remote object {key:?}"))
    }

    /// List keys below a prefix, returning paths relative to the configured base.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let location = self.location(prefix)?;
        let mut stream = self.inner.list(Some(&location));
        let mut keys = Vec::new();

        while let Some(object) = stream
            .try_next()
            .await
            .with_context(|| format!("Failed to list remote prefix {prefix:?}"))?
        {
            let relative = object.location.prefix_match(&self.base).with_context(|| {
                format!(
                    "remote store returned key {} outside configured base {}",
                    object.location, self.base
                )
            })?;
            keys.push(Path::from_iter(relative).to_string());
        }
        keys.sort_unstable();
        Ok(keys)
    }

    fn s3(url: &Url) -> Result<Self> {
        let bucket = url
            .host_str()
            .filter(|bucket| !bucket.is_empty())
            .with_context(|| format!("S3 remote URL is missing a bucket: {url}"))?;
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            bail!("S3 remote URL must contain only a bucket and path prefix: {url}");
        }

        let base = remote_prefix(url)?;
        let store = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .with_context(|| {
                format!("Failed to configure S3 remote store for bucket {bucket:?}")
            })?;
        Ok(Self {
            inner: Arc::new(store),
            base,
        })
    }

    fn local(url: &Url) -> Result<Self> {
        let root = url.to_file_path().map_err(|()| {
            anyhow::anyhow!("file remote URL must contain an absolute path: {url}")
        })?;
        if !FsPath::new(&root).is_absolute() {
            bail!("file remote URL must contain an absolute path: {url}");
        }
        std::fs::create_dir_all(&root).with_context(|| {
            format!(
                "Failed to create local remote store directory {}",
                root.display()
            )
        })?;
        let base = Path::from_filesystem_path(&root).with_context(|| {
            format!(
                "Failed to derive object-store prefix from {}",
                root.display()
            )
        })?;

        Ok(Self {
            inner: Arc::new(LocalFileSystem::new()),
            base,
        })
    }

    fn location(&self, key: &str) -> Result<Path> {
        let suffix =
            Path::parse(key).with_context(|| format!("Invalid remote object key {key:?}"))?;
        let mut location = self.base.clone();
        location.extend(&suffix);
        Ok(location)
    }
}

fn remote_prefix(url: &Url) -> Result<Path> {
    Path::from_url_path(url.path())
        .with_context(|| format!("Invalid remote URL path prefix in {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_store_round_trips_and_lists_relative_keys() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing").join("remote");
        let url = Url::from_directory_path(&root).unwrap().to_string();
        let store = RemoteStore::new(&url).unwrap();

        store
            .put("chunks/aa", Bytes::from_static(b"chunk-aa"))
            .await
            .unwrap();
        store
            .put("chunks/bb", Bytes::from_static(b"chunk-bb"))
            .await
            .unwrap();
        store
            .put(
                "sessions/s1/manifest.json",
                Bytes::from_static(b"{\"sessionId\":\"s1\"}"),
            )
            .await
            .unwrap();

        assert_eq!(
            store.get("chunks/aa").await.unwrap(),
            Bytes::from_static(b"chunk-aa")
        );
        assert_eq!(
            store.list("chunks").await.unwrap(),
            vec!["chunks/aa".to_string(), "chunks/bb".to_string()]
        );
        assert_eq!(
            store.list("sessions/s1").await.unwrap(),
            vec!["sessions/s1/manifest.json".to_string()]
        );
        assert_eq!(
            store.list("").await.unwrap(),
            vec![
                "chunks/aa".to_string(),
                "chunks/bb".to_string(),
                "sessions/s1/manifest.json".to_string()
            ]
        );
    }

    #[test]
    fn s3_store_construction_parses_bucket_and_prefix_without_network_io() {
        let store = RemoteStore::new("s3://checkpoint-bucket/team/vfs").unwrap();
        assert_eq!(store.base.as_ref(), "team/vfs");
    }

    #[test]
    fn unknown_scheme_is_rejected_clearly() {
        let error = RemoteStore::new("https://example.com/vfs")
            .err()
            .expect("unknown scheme should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported remote URL scheme \"https\""),
            "{error:#}"
        );
    }

    #[test]
    fn relative_file_url_is_rejected_clearly() {
        let error = RemoteStore::new("file:relative/path")
            .err()
            .expect("relative file URL should fail");
        assert!(
            error
                .to_string()
                .contains("file remote URL must contain an absolute path"),
            "{error:#}"
        );
    }
}
