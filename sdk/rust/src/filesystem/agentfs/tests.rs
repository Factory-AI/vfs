    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    // Turso 0.5.x reports SQLite's standard numeric value for NORMAL.
    const TURSO_OBSERVED_SYNCHRONOUS_NORMAL: i64 = 1;

    async fn create_test_fs() -> Result<(AgentFS, tempfile::TempDir)> {
        create_test_fs_with_config(CoreConfig::from_env()).await
    }

    async fn create_test_fs_with_config(
        config: CoreConfig,
    ) -> Result<(AgentFS, tempfile::TempDir)> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let pool = ConnectionPool::with_options(db, file_backed_connection_pool_options());
        let fs = AgentFS::from_pool_with_path_and_config(pool, Some(db_path), config).await?;
        Ok((fs, dir))
    }

    fn test_config_with_long_batch_window() -> CoreConfig {
        let mut config = CoreConfig::default();
        config.batcher.enabled = true;
        config.batcher.window = Duration::from_secs(60);
        config.batcher.inode_bytes = 1_048_576;
        config.batcher.global_bytes = 64 * 1024 * 1024;
        config
    }

    #[tokio::test]
    async fn core_config_batcher_enabled_flows_through_options() -> Result<()> {
        let dir = tempdir()?;

        let mut disabled = CoreConfig::default();
        disabled.batcher.enabled = false;
        let disabled_agent = crate::AgentFS::open(
            crate::AgentFSOptions::with_path(dir.path().join("disabled.db").to_string_lossy())
                .with_core_config(disabled),
        )
        .await?;
        assert!(
            disabled_agent.fs.write_batcher.is_none(),
            "AgentFSOptions CoreConfig should be able to disable the write batcher"
        );

        let enabled_agent = crate::AgentFS::open(
            crate::AgentFSOptions::with_path(dir.path().join("enabled.db").to_string_lossy())
                .with_core_config(test_config_with_long_batch_window()),
        )
        .await?;
        assert!(
            enabled_agent.fs.write_batcher.is_some(),
            "AgentFSOptions CoreConfig should be able to enable the write batcher"
        );

        Ok(())
    }

    async fn fs_inode_column_count(conn: &Connection, column_name: &str) -> Result<usize> {
        let mut rows = conn.query("PRAGMA table_info(fs_inode)", ()).await?;
        let mut count = 0;

        while let Some(row) = rows.next().await? {
            let name: String = row.get(1)?;
            if name == column_name {
                count += 1;
            }
        }

        Ok(count)
    }

    fn cached_attr(fs: &AgentFS, ino: i64) -> Option<Stats> {
        fs.attr_cache.get(ino)
    }

    fn negative_cached(fs: &AgentFS, parent_ino: i64, name: &str) -> bool {
        fs.negative_dentry_cache.contains(parent_ino, name)
    }

    async fn parent_and_name_for_test(fs: &AgentFS, path: &str) -> Result<(i64, String)> {
        let path = fs.normalize_path(path);
        let components = fs.split_path(&path);
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };
        let parent_ino = fs
            .resolve_path(&parent_path)
            .await?
            .ok_or(FsError::NotFound)?;
        Ok((parent_ino, components.last().unwrap().clone()))
    }

    async fn rename_path_via_trait(fs: &AgentFS, from: &str, to: &str) -> Result<()> {
        let (oldparent_ino, oldname) = parent_and_name_for_test(fs, from).await?;
        let (newparent_ino, newname) = parent_and_name_for_test(fs, to).await?;
        FileSystem::rename(fs, oldparent_ino, &oldname, newparent_ino, &newname).await
    }

    fn assert_normalized_ranges(actual: &[NormalizedWriteRange], expected: &[(u64, &[u8])]) {
        assert_eq!(actual.len(), expected.len());
        for (range, (offset, data)) in actual.iter().zip(expected.iter()) {
            assert_eq!(range.offset, *offset);
            assert_eq!(range.data, *data);
        }
    }

    #[test]
    fn store_characterization_range_normalization_merges_overlaps_in_order() -> Result<()> {
        let ranges = [
            WriteRangeRef {
                offset: 4,
                data: b"CCCC",
            },
            WriteRangeRef {
                offset: 0,
                data: b"aaaaaa",
            },
            WriteRangeRef {
                offset: 2,
                data: b"ZZ",
            },
            WriteRangeRef {
                offset: 8,
                data: b"!",
            },
            WriteRangeRef {
                offset: 12,
                data: b"",
            },
        ];

        let normalized = normalize_write_ranges(&ranges)?;
        assert_normalized_ranges(&normalized, &[(0, b"aaZZaaCC!")]);
        Ok(())
    }

    #[test]
    fn store_characterization_range_normalization_keeps_sparse_gaps() -> Result<()> {
        let ranges = [
            WriteRangeRef {
                offset: 0,
                data: b"ab",
            },
            WriteRangeRef {
                offset: 5,
                data: b"xy",
            },
        ];

        let normalized = normalize_write_ranges(&ranges)?;
        assert_normalized_ranges(&normalized, &[(0, b"ab"), (5, b"xy")]);
        assert!(!dense_after_inline_write_batch(0, 7, &normalized));

        let mut bridged_refs: Vec<_> = normalized
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        bridged_refs.push(WriteRangeRef {
            offset: 2,
            data: b"345",
        });

        let bridged = normalize_write_ranges(&bridged_refs)?;
        assert_normalized_ranges(&bridged, &[(0, b"ab345xy")]);
        assert!(dense_after_inline_write_batch(0, 7, &bridged));
        Ok(())
    }

    #[test]
    fn store_characterization_range_normalization_rejects_offset_overflow() {
        let ranges = [WriteRangeRef {
            offset: u64::MAX,
            data: b"x",
        }];

        match normalize_write_ranges(&ranges) {
            Ok(_) => panic!("overflowing write range should fail"),
            Err(error) => assert!(matches!(error, Error::Internal(_))),
        }
    }

    fn assert_corrupt_error(error: Error, expected_column: &str) {
        match error {
            Error::Fs(FsError::Corrupt(message)) => assert!(
                message.contains(expected_column),
                "corruption message {message:?} should mention {expected_column:?}"
            ),
            other => panic!("expected corrupt row error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn row_decode_corruption_returns_error() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/corrupt.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello").await?;
        file.drain_writes().await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            (Value::Text("not-an-integer".to_string()), created.ino),
        )
        .await?;
        fs.invalidate_attr(created.ino);

        assert_corrupt_error(
            FileSystem::getattr(&fs, created.ino).await.unwrap_err(),
            "mode",
        );
        assert_corrupt_error(
            FileSystem::lookup(&fs, ROOT_INO, "corrupt.txt")
                .await
                .unwrap_err(),
            "mode",
        );
        assert_corrupt_error(
            FileSystem::readdir_plus(&fs, ROOT_INO).await.unwrap_err(),
            "mode",
        );

        let corrupt_dir = FileSystem::mkdir(&fs, ROOT_INO, "corrupt-dir", 0o755, 0, 0).await?;
        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            (Value::Text("not-an-integer".to_string()), corrupt_dir.ino),
        )
        .await?;
        fs.invalidate_attr(corrupt_dir.ino);
        assert_corrupt_error(
            FileSystem::readdir(&fs, corrupt_dir.ino).await.unwrap_err(),
            "mode",
        );

        conn.execute(
            "UPDATE fs_inode SET mode = ?, storage_kind = ? WHERE ino = ?",
            (
                (S_IFREG | 0o644) as i64,
                Value::Text("not-an-integer".to_string()),
                created.ino,
            ),
        )
        .await?;
        fs.invalidate_attr(created.ino);

        assert_corrupt_error(file.pread(0, 5).await.unwrap_err(), "storage_kind");
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_mode_mutations_return_corrupt_without_namespace_changes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        FileSystem::mkdir(&fs, ROOT_INO, "dir", 0o755, 0, 0).await?;
        let dir = FileSystem::lookup(&fs, ROOT_INO, "dir").await?.unwrap();

        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            (Value::Text("not-an-integer".to_string()), dir.ino),
        )
        .await?;
        fs.invalidate_attr(dir.ino);

        assert_corrupt_error(
            FileSystem::unlink(&fs, ROOT_INO, "dir").await.unwrap_err(),
            "mode",
        );
        assert!(
            FileSystem::lookup(&fs, ROOT_INO, "dir").await.is_err(),
            "the corrupt dentry must still be present after the failed unlink"
        );

        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            ((S_IFDIR | 0o755) as i64, dir.ino),
        )
        .await?;
        fs.invalidate_attr(dir.ino);
        assert!(FileSystem::lookup(&fs, ROOT_INO, "dir").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_path_child_inode_returns_corrupt_and_is_not_cached() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/parent", 0, 0).await?;
        let parent = fs.stat("/parent").await?.unwrap();
        fs.mkdir("/parent/child", 0, 0).await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_dentry SET ino = ? WHERE parent_ino = ? AND name = ?",
            (
                Value::Text("not-an-integer".to_string()),
                parent.ino,
                "child",
            ),
        )
        .await?;
        fs.invalidate_dentry(parent.ino, "child");

        assert_corrupt_error(fs.stat("/parent/child").await.unwrap_err(), "ino");
        assert!(
            fs.dentry_cache.get(parent.ino, "child").is_none(),
            "corrupt child ino must not populate the dentry cache"
        );
        Ok(())
    }

    #[tokio::test]
    async fn path_api_delegates_to_trait_semantics() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        fs.mkdir("/live", 42, 43).await?;
        let live = fs.stat("/live").await?.unwrap();
        assert!(live.is_directory());
        assert_eq!((live.uid, live.gid), (42, 43));

        let (created, file) = fs
            .create_file("/live/pending.txt", DEFAULT_FILE_MODE, 7, 9)
            .await?;
        assert_eq!((created.uid, created.gid), (7, 9));
        file.pwrite(0, b"pending bytes").await?;

        let pending = fs.stat("/live/pending.txt").await?.unwrap();
        assert_eq!(
            pending.size, 13,
            "path stat must observe pending batched writes before drain"
        );
        assert_eq!(
            fs.read_file("/live/pending.txt").await?.unwrap(),
            b"pending bytes"
        );

        let (doomed, doomed_file) = fs
            .create_file("/live/doomed.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        doomed_file
            .pwrite(0, b"discard these pending bytes")
            .await?;
        drop(doomed_file);

        fs.remove("/live/doomed.txt").await?;
        fs.drain_all().await?;
        assert!(fs.stat("/live/doomed.txt").await?.is_none());
        assert_eq!(
            count_rows(&fs, "fs_data", doomed.ino).await?,
            0,
            "path remove must discard pending writes for the deleted inode"
        );

        fs.remove("/live/pending.txt").await?;
        fs.remove("/live").await?;
        assert!(fs.stat("/live").await?.is_none());

        let missing = fs.remove("/missing.txt").await.unwrap_err();
        assert!(matches!(missing, Error::Fs(FsError::NotFound)));

        fs.fsync().await?;
        Ok(())
    }

    #[tokio::test]
    async fn read_file_follows_terminal_symlink() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/target.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"target contents").await?;
        file.fsync().await?;
        FileSystem::symlink(&fs, ROOT_INO, "target.link", "target.txt", 0, 0).await?;

        let link = FileSystem::lookup(&fs, ROOT_INO, "target.link")
            .await?
            .unwrap();
        assert!(link.is_symlink());
        assert_eq!(
            fs.read_file("/target.link").await?.unwrap(),
            b"target contents"
        );

        Ok(())
    }

    #[tokio::test]
    async fn import_entries_builds_tree_with_correct_content_and_stats() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let big = vec![0xabu8; DEFAULT_INLINE_THRESHOLD + DEFAULT_CHUNK_SIZE + 17];
        let entries = [
            ImportEntry {
                path: "sub".to_string(),
                mode: S_IFDIR | 0o755,
                data: Vec::new(),
            },
            ImportEntry {
                path: "sub/inner".to_string(),
                mode: S_IFDIR | 0o755,
                data: Vec::new(),
            },
            ImportEntry {
                path: "sub/small.txt".to_string(),
                mode: S_IFREG | 0o644,
                data: b"hello import".to_vec(),
            },
            ImportEntry {
                path: "sub/inner/big.bin".to_string(),
                mode: S_IFREG | 0o755,
                data: big.clone(),
            },
            ImportEntry {
                path: "sub/link".to_string(),
                mode: S_IFLNK | 0o777,
                data: b"small.txt".to_vec(),
            },
        ];
        let opts = ImportOptions {
            uid: 7,
            gid: 9,
            timestamp: (1_700_000_000, 123_456_789),
        };
        let imported = fs.import_entries(ROOT_INO, &entries, &opts).await?;
        assert_eq!(imported.len(), entries.len());

        assert_eq!(
            fs.read_file("/sub/small.txt").await?.unwrap(),
            b"hello import"
        );
        assert_eq!(fs.read_file("/sub/inner/big.bin").await?.unwrap(), big);
        assert_eq!(fs.readlink("/sub/link").await?.unwrap(), "small.txt");

        let small = fs.stat("/sub/small.txt").await?.unwrap();
        let reported = imported.iter().find(|e| e.path == "sub/small.txt").unwrap();
        assert_eq!(small.ino, reported.ino);
        assert_eq!(small.size as u64, reported.size);
        assert_eq!(small.mode, S_IFREG | 0o644);
        assert_eq!((small.uid, small.gid), (7, 9));
        assert_eq!(small.mtime, 1_700_000_000);
        assert_eq!(small.mtime_nsec, 123_456_789);
        assert_eq!(small.ctime, 1_700_000_000);

        let big_stat = fs.stat("/sub/inner/big.bin").await?.unwrap();
        assert_eq!(big_stat.size as usize, big.len());
        assert_eq!(big_stat.mode, S_IFREG | 0o755);

        let sub = fs.stat("/sub").await?.unwrap();
        assert_eq!(sub.nlink, 3); // "." + parent link + inner

        // Duplicate import collides on the dentry UNIQUE constraint.
        let dup = fs.import_entries(ROOT_INO, &entries[..1], &opts).await;
        assert!(matches!(dup, Err(Error::Fs(FsError::AlreadyExists))));

        Ok(())
    }

    #[tokio::test]
    async fn import_session_preserves_tree_and_chunk_limits() -> Result<()> {
        let mut config = CoreConfig::default();
        config.batcher.txn_max_inodes = 2;
        config.batcher.txn_max_bytes = usize::MAX;
        let (fs, _dir) = create_test_fs_with_config(config).await?;

        let big = vec![0xcdu8; DEFAULT_INLINE_THRESHOLD + DEFAULT_CHUNK_SIZE + 11];
        let entries = [
            ImportEntry {
                path: "tree".to_string(),
                mode: S_IFDIR | 0o755,
                data: Vec::new(),
            },
            ImportEntry {
                path: "tree/nested".to_string(),
                mode: S_IFDIR | 0o700,
                data: Vec::new(),
            },
            ImportEntry {
                path: "tree/small.txt".to_string(),
                mode: S_IFREG | 0o640,
                data: b"small import".to_vec(),
            },
            ImportEntry {
                path: "tree/nested/big.bin".to_string(),
                mode: S_IFREG | 0o600,
                data: big.clone(),
            },
            ImportEntry {
                path: "tree/link".to_string(),
                mode: S_IFLNK | 0o777,
                data: b"nested/big.bin".to_vec(),
            },
        ];
        let opts = ImportOptions {
            uid: 111,
            gid: 222,
            timestamp: (1_800_000_000, 987_654_321),
        };

        let commit_start = fs.import_commit_sizes.lock().unwrap().len();
        let mut session = fs.begin_import(ROOT_INO, opts.clone()).await?;
        session.import_chunk(&entries[..2]).await?;
        session.import_chunk(&entries[2..]).await?;
        let imported = session.finish();
        let commit_sizes = {
            let sizes = fs.import_commit_sizes.lock().unwrap();
            sizes[commit_start..].to_vec()
        };

        assert_eq!(imported.len(), entries.len());
        assert_eq!(
            commit_sizes,
            vec![2, 2, 1],
            "two explicit import chunks should commit as 2 + 2 + 1 inode transactions"
        );
        println!(
            "imported {} entries in transaction chunks {:?}",
            imported.len(),
            commit_sizes
        );

        let tree = fs.stat("/tree").await?.unwrap();
        assert_eq!(tree.mode, S_IFDIR | 0o755);
        assert_eq!(tree.nlink, 3);
        assert_eq!((tree.uid, tree.gid), (111, 222));
        assert_eq!(tree.mtime, opts.timestamp.0);
        assert_eq!(tree.mtime_nsec, opts.timestamp.1 as u32);

        let small = fs.stat("/tree/small.txt").await?.unwrap();
        let small_reported = imported
            .iter()
            .find(|entry| entry.path == "tree/small.txt")
            .unwrap();
        assert_eq!(small.ino, small_reported.ino);
        assert_eq!(small.mode, S_IFREG | 0o640);
        assert_eq!(small.size as u64, small_reported.size);
        assert_eq!(
            fs.read_file("/tree/small.txt").await?.unwrap(),
            b"small import"
        );

        let big_stat = fs.stat("/tree/nested/big.bin").await?.unwrap();
        let big_reported = imported
            .iter()
            .find(|entry| entry.path == "tree/nested/big.bin")
            .unwrap();
        assert_eq!(big_stat.ino, big_reported.ino);
        assert_eq!(big_stat.mode, S_IFREG | 0o600);
        assert_eq!(big_stat.size as usize, big.len());
        assert_eq!(fs.read_file("/tree/nested/big.bin").await?.unwrap(), big);
        assert_eq!(fs.readlink("/tree/link").await?.unwrap(), "nested/big.bin");
        Ok(())
    }

    #[tokio::test]
    async fn attr_cache_invalidates_mutations_and_preserves_visibility() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        assert!(cached_attr(&fs, ROOT_INO).is_some());

        let (created, file) =
            FileSystem::create_file(&fs, ROOT_INO, "cache.txt", DEFAULT_FILE_MODE, 7, 9).await?;
        let file_ino = created.ino;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert_eq!(cached_attr(&fs, file_ino).unwrap().size, 0);

        file.pwrite(0, b"hello").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let written = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(written.size, 5);
        assert_eq!(cached_attr(&fs, file_ino).unwrap().size, 5);

        file.pwrite(5, b" world").await?;
        let after_append = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(after_append.size, 11);
        assert_eq!(file.pread(0, 11).await?, b"hello world");

        file.truncate(5).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let truncated = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(truncated.size, 5);
        assert_eq!(file.pread(0, 16).await?, b"hello");

        FileSystem::chmod(&fs, file_ino, 0o600).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let chmodded = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(chmodded.mode & 0o7777, 0o600);

        FileSystem::chown(&fs, file_ino, Some(11), Some(13)).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let chowned = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!((chowned.uid, chowned.gid), (11, 13));

        FileSystem::utimens(
            &fs,
            file_ino,
            TimeChange::Set(1_700_000_001, 123),
            TimeChange::Set(1_700_000_002, 456),
        )
        .await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let timestamped = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(
            (timestamped.mtime, timestamped.mtime_nsec),
            (1_700_000_002, 456)
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let linked = FileSystem::link(&fs, file_ino, ROOT_INO, "hard.txt").await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert_eq!(linked.nlink, 2);
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "hard.txt")
                .await?
                .unwrap()
                .ino,
            file_ino
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let symlink = FileSystem::symlink(&fs, ROOT_INO, "cache.link", "cache.txt", 11, 13).await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(symlink.is_symlink());
        assert_eq!(
            FileSystem::readlink(&fs, symlink.ino).await?,
            Some("cache.txt".to_string())
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let dir = FileSystem::mkdir(&fs, ROOT_INO, "dir", 0o755, 11, 13).await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(cached_attr(&fs, dir.ino).is_some());
        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        FileSystem::rmdir(&fs, ROOT_INO, "dir").await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(cached_attr(&fs, dir.ino).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "dir").await?.is_none());

        FileSystem::getattr(&fs, file_ino).await?.unwrap();
        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        FileSystem::rename(&fs, ROOT_INO, "cache.txt", ROOT_INO, "renamed.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "cache.txt")
            .await?
            .is_none());
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
                .await?
                .unwrap()
                .ino,
            file_ino
        );
        assert_eq!(file.pread(0, 16).await?, b"hello");

        FileSystem::getattr(&fs, file_ino).await?.unwrap();
        FileSystem::unlink(&fs, ROOT_INO, "hard.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let single_link = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(single_link.nlink, 1);

        FileSystem::unlink(&fs, ROOT_INO, "renamed.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
            .await?
            .is_none());

        Ok(())
    }

    #[tokio::test]
    async fn negative_dentry_cache_invalidates_on_namespace_mutations() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert!(FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
            .await?
            .is_none());
        assert!(negative_cached(&fs, ROOT_INO, "missing.txt"));

        let (created, _file) =
            FileSystem::create_file(&fs, ROOT_INO, "missing.txt", DEFAULT_FILE_MODE, 7, 9).await?;
        assert!(!negative_cached(&fs, ROOT_INO, "missing.txt"));
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
                .await?
                .unwrap()
                .ino,
            created.ino
        );

        FileSystem::rename(&fs, ROOT_INO, "missing.txt", ROOT_INO, "renamed.txt").await?;
        assert!(negative_cached(&fs, ROOT_INO, "missing.txt"));
        assert!(!negative_cached(&fs, ROOT_INO, "renamed.txt"));
        assert!(FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
            .await?
            .is_none());
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
                .await?
                .unwrap()
                .ino,
            created.ino
        );

        FileSystem::unlink(&fs, ROOT_INO, "renamed.txt").await?;
        assert!(negative_cached(&fs, ROOT_INO, "renamed.txt"));
        assert!(FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
            .await?
            .is_none());

        assert!(FileSystem::lookup(&fs, ROOT_INO, "negdir").await?.is_none());
        assert!(negative_cached(&fs, ROOT_INO, "negdir"));
        FileSystem::mkdir(&fs, ROOT_INO, "negdir", 0o755, 7, 9).await?;
        assert!(!negative_cached(&fs, ROOT_INO, "negdir"));
        FileSystem::rmdir(&fs, ROOT_INO, "negdir").await?;
        assert!(negative_cached(&fs, ROOT_INO, "negdir"));

        Ok(())
    }

    async fn read_pragma_i64(conn: &Connection, sql: &str) -> i64 {
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get_value(0)
            .ok()
            .and_then(|value| match value {
                Value::Integer(value) => Some(value),
                Value::Text(value) => value.parse().ok(),
                _ => None,
            })
            .unwrap()
    }

    async fn read_pragma_text(conn: &Connection, sql: &str) -> String {
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get_value(0)
            .ok()
            .and_then(|value| match value {
                Value::Text(value) => Some(value.clone()),
                Value::Integer(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap()
    }

    // ==================== Chunk Size Boundary Tests ====================

    #[tokio::test]
    async fn test_file_smaller_than_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file smaller than chunk_size (100 bytes)
        let data = vec![0u8; 100];
        let (_, file) = fs
            .create_file("/small.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/small.txt").await?.unwrap();
        assert_eq!(read_data.len(), 100);
        assert_eq!(read_data, data);

        // Verify inline storage avoids chunks
        let ino = fs.resolve_path("/small.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(data));

        Ok(())
    }

    #[tokio::test]
    async fn test_file_exactly_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write exactly chunk_size bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..chunk_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/exact.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/exact.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size);
        assert_eq!(read_data, data);

        // Verify only 1 chunk was created
        let ino = fs.resolve_path("/exact.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_one_byte_over_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write chunk_size + 1 bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..=chunk_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/overflow.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/overflow.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size + 1);
        assert_eq!(read_data, data);

        // Verify 2 chunks were created
        let ino = fs.resolve_path("/overflow.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_spanning_multiple_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write ~2.5 chunks worth of data
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 2 + chunk_size / 2;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/multi.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/multi.txt").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify 3 chunks were created
        let ino = fs.resolve_path("/multi.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    // ==================== Data Integrity Tests ====================

    #[tokio::test]
    async fn test_roundtrip_byte_for_byte() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create data that spans chunk boundaries with identifiable patterns
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 3 + 123; // Odd size spanning 4 chunks

        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/roundtrip.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/roundtrip.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data, "Data mismatch after roundtrip");

        Ok(())
    }

    #[tokio::test]
    async fn test_binary_data_with_null_bytes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create data with null bytes at chunk boundaries
        let mut data = vec![0u8; chunk_size * 2 + 100];
        // Put nulls at the chunk boundary
        data[chunk_size - 1] = 0;
        data[chunk_size] = 0;
        data[chunk_size + 1] = 0;
        // Put some non-null bytes around
        data[chunk_size - 2] = 0xFF;
        data[chunk_size + 2] = 0xFF;

        let (_, file) = fs
            .create_file("/nulls.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;
        let read_data = fs.read_file("/nulls.bin").await?.unwrap();

        assert_eq!(read_data, data, "Null bytes at chunk boundary corrupted");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create sequential bytes spanning multiple chunks
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/sequential.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/sequential.bin").await?.unwrap();

        // Verify every byte is in the correct position
        for (i, (&expected, &actual)) in data.iter().zip(read_data.iter()).enumerate() {
            assert_eq!(
                expected, actual,
                "Byte mismatch at position {}: expected {}, got {}",
                i, expected, actual
            );
        }

        Ok(())
    }

    // ==================== Edge Case Tests ====================

    #[tokio::test]
    async fn test_empty_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write empty file
        let (_, file) = fs
            .create_file("/empty.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &[]).await?;

        // Read it back
        let read_data = fs.read_file("/empty.txt").await?.unwrap();
        assert!(read_data.is_empty());

        // Verify 0 chunks were created
        let ino = fs.resolve_path("/empty.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 0);

        // Verify size is 0
        let stats = fs.stat("/empty.txt").await?.unwrap();
        assert_eq!(stats.size, 0);

        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(Vec::new()));

        Ok(())
    }

    #[tokio::test]
    async fn test_inline_small_file_and_overwrite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/inline.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello world").await?;
        file.pwrite(6, b"agent").await?;

        let ino = fs.resolve_path("/inline.txt").await?.unwrap();
        assert_eq!(fs.read_file("/inline.txt").await?.unwrap(), b"hello agent");
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(b"hello agent".to_vec()));

        Ok(())
    }

    #[tokio::test]
    async fn test_inline_transitions_to_chunked_over_threshold() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let prefix = vec![1u8; DEFAULT_INLINE_THRESHOLD];
        let suffix = vec![2u8; 32];
        let (_, file) = fs
            .create_file("/transition.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &prefix).await?;

        let ino = fs.resolve_path("/transition.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        file.pwrite(DEFAULT_INLINE_THRESHOLD as u64, &suffix)
            .await?;

        let mut expected = prefix;
        expected.extend_from_slice(&suffix);
        assert_eq!(fs.read_file("/transition.bin").await?.unwrap(), expected);
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_sparse_write_transitions_inline_to_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/sparse.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"abc").await?;
        file.pwrite(10, b"z").await?;

        let ino = fs.resolve_path("/sparse.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);

        let mut expected = b"abc".to_vec();
        expected.resize(10, 0);
        expected.push(b'z');
        let read_back = file.pread(0, expected.len() as u64).await?;
        assert_eq!(read_back, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunked_truncate_back_to_inline_when_dense() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let data = vec![7u8; DEFAULT_INLINE_THRESHOLD + 1];
        let (_, file) = fs
            .create_file("/dense.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/dense.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));

        file.truncate(128).await?;

        assert_eq!(fs.read_file("/dense.bin").await?.unwrap(), vec![7u8; 128]);
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(vec![7u8; 128]));

        Ok(())
    }

    #[tokio::test]
    async fn test_sparse_chunked_truncate_below_threshold_stays_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/sparse-truncate.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(fs.chunk_size() as u64 + 8, b"tail").await?;
        // Tier Four: ensure the sparse write reaches SQLite as chunked
        // storage before we truncate; otherwise truncate_pending strips it
        // in memory and the file never transitions out of INLINE.
        file.fsync().await?;
        file.truncate(4).await?;

        let ino = fs.resolve_path("/sparse-truncate.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(file.pread(0, 4).await?, vec![0u8; 4]);

        Ok(())
    }

    #[tokio::test]
    async fn test_64k_chunk_boundary_uses_single_default_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert_eq!(fs.chunk_size(), 64 * 1024);
        let data: Vec<u8> = (0..fs.chunk_size()).map(|i| (i % 251) as u8).collect();
        let (_, file) = fs
            .create_file("/boundary.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/boundary.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);
        assert_eq!(
            file.pread((fs.chunk_size() - 8) as u64, 16).await?,
            data[fs.chunk_size() - 8..].to_vec()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial large file (3 chunks)
        let initial_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/overwrite.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &initial_data).await?;

        let ino = fs.resolve_path("/overwrite.txt").await?.unwrap();
        let initial_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(initial_chunk_count, 3);

        // Overwrite with smaller file (1 chunk)
        let new_data = vec![42u8; 100];
        file.truncate(0).await?;
        file.pwrite(0, &new_data).await?;

        // Verify old chunks are gone and new data is correct
        let read_data = fs.read_file("/overwrite.txt").await?.unwrap();
        assert_eq!(read_data, new_data);

        let new_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(new_chunk_count, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(new_data));

        // Verify size is updated
        let stats = fs.stat("/overwrite.txt").await?.unwrap();
        assert_eq!(stats.size, 100);

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_with_larger_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial small file (1 chunk)
        let initial_data = vec![1u8; 100];
        let (_, file) = fs.create_file("/grow.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &initial_data).await?;

        let ino = fs.resolve_path("/grow.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        // Overwrite with larger file (3 chunks)
        let new_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        file.truncate(0).await?;
        file.pwrite(0, &new_data).await?;

        // Verify data is correct
        let read_data = fs.read_file("/grow.txt").await?.unwrap();
        assert_eq!(read_data, new_data);
        assert_eq!(fs.get_chunk_count(ino).await?, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_very_large_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write 1MB file
        let data_size = 1024 * 1024;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/large.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/large.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify correct number of chunks
        let chunk_size = fs.chunk_size();
        let expected_chunks = data_size.div_ceil(chunk_size);
        let ino = fs.resolve_path("/large.bin").await?.unwrap();
        let actual_chunks = fs.get_chunk_count(ino).await? as usize;
        assert_eq!(actual_chunks, expected_chunks);

        Ok(())
    }

    // ==================== Configuration Tests ====================

    #[tokio::test]
    async fn test_default_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert_eq!(fs.chunk_size(), DEFAULT_CHUNK_SIZE);
        assert_eq!(fs.chunk_size(), 65536);
        assert_eq!(fs.inline_threshold(), DEFAULT_INLINE_THRESHOLD);
        assert_eq!(fs.inline_threshold(), 16384);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_size_accessor() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        assert!(chunk_size > 0);

        // Write data and verify chunks match expected based on chunk_size
        let data = vec![0u8; chunk_size * 2 + 1];
        let (_, file) = fs.create_file("/test.bin", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/test.bin").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_config_persistence() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Query fs_config table directly
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        let row = rows.next().await?.expect("chunk_size config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("chunk_size should be a text value");

        assert_eq!(value, "65536");

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'inline_threshold'",
                (),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .expect("inline_threshold config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("inline_threshold should be a text value");

        assert_eq!(value, "16384");

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = ?",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .expect("schema version config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("schema version should be a text value");

        assert_eq!(value, "0.5");

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_non_duplicate_errors_propagate() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("malformed-view.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE VIEW fs_inode AS SELECT 1 AS ino", ())
            .await?;

        let err = match schema::ensure_current(&conn).await {
            Ok(_) => panic!("non-duplicate schema DDL errors must propagate"),
            Err(err) => err,
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("fs_inode"),
            "error should preserve the failed DDL target, got: {err_msg}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_conflicting_column_definition_is_rejected() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("malformed-conflicting-column.db");

        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await?;
            let conn = db.connect()?;

            conn.execute(
                "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES
                    (?, '0.5'),
                    (?, '16384')",
                (
                    schema::CONFIG_SCHEMA_VERSION_KEY,
                    schema::CONFIG_INLINE_THRESHOLD_KEY,
                ),
            )
            .await?;
            conn.execute(
                "CREATE TABLE fs_inode (
                    ino INTEGER PRIMARY KEY,
                    mode INTEGER NOT NULL,
                    nlink INTEGER NOT NULL DEFAULT 0,
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL,
                    rdev INTEGER NOT NULL DEFAULT 0,
                    atime_nsec TEXT NOT NULL DEFAULT 'bad',
                    mtime_nsec INTEGER NOT NULL DEFAULT 0,
                    ctime_nsec INTEGER NOT NULL DEFAULT 0,
                    data_inline BLOB,
                    storage_kind INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_inode
                    (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                     atime_nsec, mtime_nsec, ctime_nsec, storage_kind)
                 VALUES (?, ?, 2, 0, 0, 0, 1, 1, 1, 0, 'bad', 0, 0, 0)",
                (ROOT_INO, DEFAULT_DIR_MODE as i64),
            )
            .await?;
        }

        let err = match AgentFS::new(db_path.to_str().unwrap()).await {
            Ok(_) => panic!("opening a database with a conflicting schema column must fail"),
            Err(err) => err,
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("fs_inode.atime_nsec") && err_msg.contains("incompatible definition"),
            "error should name the incompatible schema column, got: {err_msg}"
        );

        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        let mut rows = conn.query("PRAGMA table_info(fs_inode)", ()).await?;
        let mut found_conflicting_column = false;
        while let Some(row) = rows.next().await? {
            let name: String = row.get(1)?;
            if name == "atime_nsec" {
                let type_name: String = row.get(2)?;
                assert_eq!(type_name, "TEXT");
                found_conflicting_column = true;
            }
        }
        assert!(found_conflicting_column);

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_duplicate_columns_are_idempotent_on_reopen() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("already-upgraded.db");

        let first = AgentFS::new(db_path.to_str().unwrap()).await?;
        drop(first);

        let reopened = AgentFS::new(db_path.to_str().unwrap()).await?;
        let conn = reopened.pool.get_connection().await?;

        for column_name in [
            "atime_nsec",
            "mtime_nsec",
            "ctime_nsec",
            "data_inline",
            "storage_kind",
        ] {
            assert_eq!(
                fs_inode_column_count(&conn, column_name).await?,
                1,
                "fs_inode should contain exactly one {column_name} column"
            );
        }

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = ?",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
            )
            .await?;
        let version: String = rows
            .next()
            .await?
            .expect("schema version config should exist")
            .get(0)?;
        assert_eq!(version, schema::AGENTFS_SCHEMA_VERSION);

        Ok(())
    }

    #[tokio::test]
    async fn test_v04_database_migrates_to_current_on_open() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("legacy-v04.db");

        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await?;
            let conn = db.connect()?;
            conn.execute(
                "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES (?, '0.4')",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
            )
            .await?;
            conn.execute(
                "CREATE TABLE fs_inode (
                    ino INTEGER PRIMARY KEY AUTOINCREMENT,
                    mode INTEGER NOT NULL,
                    nlink INTEGER NOT NULL DEFAULT 0,
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL,
                    rdev INTEGER NOT NULL DEFAULT 0,
                    atime_nsec INTEGER NOT NULL DEFAULT 0,
                    mtime_nsec INTEGER NOT NULL DEFAULT 0,
                    ctime_nsec INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await?;
        }

        let agent =
            crate::AgentFS::open(crate::AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await?;
        let conn = agent.get_connection().await?;
        assert_eq!(
            schema::detect_schema_version(&conn).await?,
            Some(schema::CURRENT)
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_file_backed_connections_use_production_pragmas() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let conn1 = fs.pool.get_connection().await?;
        let conn2 = fs.pool.get_connection().await?;

        for conn in [&conn1, &conn2] {
            assert_eq!(
                read_pragma_i64(conn, "PRAGMA synchronous").await,
                TURSO_OBSERVED_SYNCHRONOUS_NORMAL
            );
            assert_eq!(read_pragma_i64(conn, "PRAGMA busy_timeout").await, 5000);
            assert_eq!(read_pragma_i64(conn, "PRAGMA temp_store").await, 2);
            assert_eq!(
                read_pragma_text(conn, "PRAGMA journal_mode")
                    .await
                    .to_lowercase(),
                "wal"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_file_backed_options_issue_durable_baseline_sql() {
        let options = file_backed_connection_pool_options();

        assert_eq!(options.max_connections, FILE_BACKED_MAX_CONNECTIONS);
        assert_eq!(options.setup_sql[0], TEMP_STORE_MEMORY_SQL);
        assert!(options.setup_sql.iter().any(|sql| sql == BUSY_TIMEOUT_SQL));
        assert!(options.setup_sql.iter().any(|sql| sql == WAL_MODE_SQL));
        assert!(options
            .setup_sql
            .iter()
            .any(|sql| sql == BASELINE_SYNCHRONOUS_SQL));
        assert!(!options
            .setup_sql
            .iter()
            .any(|sql| sql == "PRAGMA synchronous = OFF"));
    }

    #[tokio::test]
    async fn test_memory_agentfs_connections_use_temp_store_memory() -> Result<()> {
        let agentfs = crate::AgentFS::open(crate::AgentFSOptions::ephemeral()).await?;

        let conn = agentfs.get_connection().await?;
        assert_eq!(read_pragma_i64(&conn, "PRAGMA temp_store").await, 2);
        drop(conn);

        let core_agentfs = AgentFS::new(":memory:").await?;
        let core_conn = core_agentfs.pool.get_connection().await?;
        assert_eq!(read_pragma_i64(&core_conn, "PRAGMA temp_store").await, 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_backed_agentfs_concurrent_operations_complete() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/seed.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"seed").await?;

        let mut handles = Vec::new();
        for worker in 0..8 {
            let fs = fs.clone();
            handles.push(tokio::spawn(async move {
                for iteration in 0..5 {
                    let data = fs.read_file("/seed.txt").await?.unwrap();
                    assert_eq!(data, b"seed");

                    let path = format!("/worker-{worker}-{iteration}");
                    fs.mkdir(&path, 0, 0).await?;
                }
                Ok::<(), Error>(())
            }));
        }

        for handle in handles {
            handle.await.unwrap()?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_fsync_restores_synchronous_normal() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = OFF", ()).await?;
        drop(conn);

        fs.fsync().await?;

        let conn = fs.pool.get_connection().await?;
        assert_eq!(
            read_pragma_i64(&conn, "PRAGMA synchronous").await,
            TURSO_OBSERVED_SYNCHRONOUS_NORMAL
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_file_fsync_restores_synchronous_normal() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/fsync.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = OFF", ()).await?;
        drop(conn);

        file.fsync().await?;

        let conn = fs.pool.get_connection().await?;
        assert_eq!(
            read_pragma_i64(&conn, "PRAGMA synchronous").await,
            TURSO_OBSERVED_SYNCHRONOUS_NORMAL
        );

        Ok(())
    }

    // ==================== Schema Tests ====================

    #[tokio::test]
    async fn test_chunk_index_uniqueness() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file to create chunks
        let chunk_size = fs.chunk_size();
        let data = vec![0u8; chunk_size * 2];
        let (_, file) = fs
            .create_file("/unique.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/unique.txt").await?.unwrap();
        // Tier Four: pwrite is async-batched; drain so fs_data is populated
        // before we probe its primary-key constraint.
        fs.drain_inode_writes(ino).await?;

        // Try to insert a duplicate chunk - should fail due to PRIMARY KEY constraint
        let conn = fs.pool.get_connection().await?;
        let result = conn
            .execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, 0, ?)",
                (ino, vec![1u8; 10]),
            )
            .await;

        assert!(result.is_err(), "Duplicate chunk_index should be rejected");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering_in_database() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create 5 chunks with identifiable data
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/ordered.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/ordered.bin").await?.unwrap();
        // Tier Four: drain so fs_data rows are present for the SELECT below.
        fs.drain_inode_writes(ino).await?;

        // Query chunks in order
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT chunk_index FROM fs_data WHERE ino = ? ORDER BY chunk_index",
                (ino,),
            )
            .await?;

        let mut indices = Vec::new();
        while let Some(row) = rows.next().await? {
            let idx = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(-1);
            indices.push(idx);
        }

        assert_eq!(indices, vec![0, 1, 2, 3, 4]);

        Ok(())
    }

    // ==================== Cleanup Tests ====================

    #[tokio::test]
    async fn test_delete_file_removes_all_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create multi-chunk file
        let data = vec![0u8; chunk_size * 4];
        let (_, file) = fs
            .create_file("/deleteme.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/deleteme.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 4);

        // Close the handle first: with it open, deletion is deferred (POSIX
        // unlink-while-open) and the chunks legitimately survive the remove.
        drop(file);

        // Delete the file
        fs.remove("/deleteme.txt").await?;

        // Verify all chunks are gone
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
            .await?;

        let count = rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1);

        assert_eq!(count, 0, "All chunks should be deleted");

        Ok(())
    }

    async fn count_rows(fs: &AgentFS, table: &str, ino: i64) -> Result<i64> {
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query(
                &format!("SELECT COUNT(*) FROM {table} WHERE ino = ?"),
                (ino,),
            )
            .await?;
        Ok(rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1))
    }

    #[tokio::test]
    async fn test_unlink_while_open_defers_reap() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (stats, file) = fs
            .create_file("/ghost.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = stats.ino;
        file.pwrite(0, b"ghost").await?;

        FileSystem::unlink(&fs, ROOT_INO, "ghost.bin").await?;

        // POSIX: the open handle keeps the inode readable and writable.
        assert!(fs.resolve_path("/ghost.bin").await?.is_none());
        assert_eq!(file.pread(0, 5).await?, b"ghost");
        file.pwrite(5, b"-more").await?;
        assert_eq!(file.pread(0, 10).await?, b"ghost-more");
        assert_eq!(file.fstat().await?.nlink, 0);
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 1);

        // Last handle drop queues the reap; the next namespace mutation
        // (or finalize) executes it.
        drop(file);
        fs.process_deferred_reaps().await?;
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[derive(Default)]
    struct FailOnceReapHook {
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ReapHook for FailOnceReapHook {
        async fn on_reap(&self, conn: &Connection, ino: i64) -> Result<()> {
            conn.execute("INSERT INTO reap_hook_probe (ino) VALUES (?)", (ino,))
                .await?;
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::Internal("intentional reap hook failure".to_string()));
            }
            Ok(())
        }
    }

    struct PendingStateReapHook {
        batcher: Arc<AgentFSWriteBatcher>,
        saw_pending: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ReapHook for PendingStateReapHook {
        async fn on_reap(&self, _conn: &Connection, ino: i64) -> Result<()> {
            self.saw_pending.store(
                self.batcher.has_pending(ino),
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(())
        }
    }

    async fn count_probe_rows(fs: &AgentFS, ino: i64) -> Result<i64> {
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM reap_hook_probe WHERE ino = ?", (ino,))
            .await?;
        Ok(rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1))
    }

    #[tokio::test]
    async fn lifecycle_reap_hook_fires_atomically() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let conn = fs.pool.get_connection().await?;
        conn.execute("CREATE TABLE reap_hook_probe (ino INTEGER PRIMARY KEY)", ())
            .await?;
        drop(conn);

        fs.register_reap_hook(Arc::new(FailOnceReapHook::default()));

        let (stats, file) = fs
            .create_file("/hooked.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = stats.ino;
        file.pwrite(0, b"hooked").await?;

        FileSystem::unlink(&fs, ROOT_INO, "hooked.bin").await?;
        assert_eq!(file.pread(0, 6).await?, b"hooked");
        drop(file);

        let err = fs
            .process_deferred_reaps()
            .await
            .expect_err("first hook invocation should fail");
        assert!(
            err.to_string().contains("intentional reap hook failure"),
            "unexpected reap hook error: {err}"
        );
        assert_eq!(
            count_rows(&fs, "fs_inode", ino).await?,
            1,
            "failed hook must roll back the inode deletion"
        );
        assert_eq!(
            count_probe_rows(&fs, ino).await?,
            0,
            "hook writes must be in the same transaction as the reap"
        );

        fs.process_deferred_reaps().await?;
        assert_eq!(count_probe_rows(&fs, ino).await?, 1);
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn deferred_reap_discards_pending_before_reap_hooks() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/pending-reap.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = stats.ino;
        file.pwrite(0, b"pending reap").await?;

        let batcher = fs
            .write_batcher
            .as_ref()
            .expect("long-window test config enables the batcher")
            .clone();
        assert!(
            batcher.has_pending(ino),
            "test setup requires a pending write before the file is reaped"
        );

        let hook = Arc::new(PendingStateReapHook {
            batcher: Arc::clone(&batcher),
            saw_pending: std::sync::atomic::AtomicBool::new(true),
        });
        fs.register_reap_hook(hook.clone());

        FileSystem::unlink(&fs, ROOT_INO, "pending-reap.bin").await?;
        drop(file);
        fs.process_deferred_reaps().await?;

        assert!(
            !hook.saw_pending.load(std::sync::atomic::Ordering::SeqCst),
            "pending writes must be discarded before reap hooks run in the reap transaction"
        );
        assert!(
            !batcher.has_pending(ino),
            "pending writes for a reaped inode must not survive the reap"
        );
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_mount_sweep_reaps_crashed_orphans() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("sweep.db");
        let db_path = db_path.to_str().unwrap();

        let ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/ghost.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"ghost").await?;
            file.drain_writes().await?;
            FileSystem::unlink(&fs, ROOT_INO, "ghost.bin").await?;
            // Simulate a crash: the guard never releases, so the orphan is
            // neither queued nor reaped before the process "dies".
            std::mem::forget(file);
            stats.ino
        };

        let fs = AgentFS::new(db_path).await?;
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_reaps_open_unlink_and_mount_sweep() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("lifecycle.db");
        let db_path = db_path.to_str().unwrap();

        let deferred_ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/deferred.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"deferred").await?;
            FileSystem::unlink(&fs, ROOT_INO, "deferred.bin").await?;
            assert!(fs.resolve_path("/deferred.bin").await?.is_none());
            assert_eq!(file.pread(0, 8).await?, b"deferred");
            assert_eq!(file.fstat().await?.nlink, 0);
            drop(file);
            fs.process_deferred_reaps().await?;
            stats.ino
        };

        let crashed_ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/crashed.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"crashed").await?;
            file.drain_writes().await?;
            FileSystem::unlink(&fs, ROOT_INO, "crashed.bin").await?;
            std::mem::forget(file);
            stats.ino
        };

        let fs = AgentFS::new(db_path).await?;
        for ino in [deferred_ino, crashed_ino] {
            assert_eq!(
                count_rows(&fs, "fs_inode", ino).await?,
                0,
                "reaped inode {ino} should not remain in fs_inode"
            );
            assert_eq!(
                count_rows(&fs, "fs_data", ino).await?,
                0,
                "reaped inode {ino} should not leave fs_data rows"
            );
            assert_eq!(
                count_rows(&fs, "fs_symlink", ino).await?,
                0,
                "reaped inode {ino} should not leave fs_symlink rows"
            );
        }

        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_inode WHERE nlink = 0", ())
            .await?;
        let nlink_zero = rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1);
        assert_eq!(nlink_zero, 0, "mount sweep should leave no nlink=0 rows");

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_files_different_sizes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Create files of various sizes
        let files = vec![
            ("/tiny.txt", 10),
            ("/small.txt", chunk_size / 2),
            ("/exact.txt", chunk_size),
            ("/medium.txt", chunk_size * 2 + 100),
            ("/large.txt", chunk_size * 5),
        ];

        for (path, size) in &files {
            let data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            let (_, file) = fs.create_file(path, DEFAULT_FILE_MODE, 0, 0).await?;
            file.pwrite(0, &data).await?;
        }

        // Verify each file has correct data and chunk count
        for (path, size) in &files {
            let read_data = fs.read_file(path).await?.unwrap();
            assert_eq!(read_data.len(), *size, "Size mismatch for {}", path);

            let expected_data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            assert_eq!(read_data, expected_data, "Data mismatch for {}", path);

            let expected_chunks = if *size <= fs.inline_threshold() {
                0
            } else {
                size.div_ceil(chunk_size)
            };
            let ino = fs.resolve_path(path).await?.unwrap();
            let actual_chunks = fs.get_chunk_count(ino).await? as usize;
            assert_eq!(
                actual_chunks, expected_chunks,
                "Chunk count mismatch for {}",
                path
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn file_pread_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        assert_eq!(file.pread(0, 10).await?, &data[0..10]);
        assert_eq!(file.pread(50, 20).await?, &data[50..70]);
        assert_eq!(file.pread(90, 10).await?, &data[90..100]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_past_eof() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        assert!(file.pread(100, 10).await?.is_empty());
        assert_eq!(file.pread(40, 20).await?, &data[40..50]);
        Ok(())
    }

    #[tokio::test]
    async fn file_open_nonexistent_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = fs.open("/nonexistent.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let start = chunk_size - 10;
        assert_eq!(
            file.pread(start as u64, 20).await?,
            &data[start..start + 20]
        );

        let start = chunk_size / 2;
        let size = chunk_size * 2;
        assert_eq!(
            file.pread(start as u64, size as u64).await?,
            &data[start..start + size]
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = vec![0; 100];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.pwrite(50, &[1, 2, 3, 4, 5]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[50..55], &[1, 2, 3, 4, 5]);
        assert_eq!(&result[0..50], &vec![0u8; 50][..]);
        assert_eq!(&result[55..100], &vec![0u8; 45][..]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = vec![1; 50];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.pwrite(100, &[2, 2, 2, 2, 2]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 105);
        assert_eq!(&result[0..50], &vec![1u8; 50][..]);
        assert_eq!(&result[50..100], &vec![0u8; 50][..]);
        assert_eq!(&result[100..105], &[2, 2, 2, 2, 2]);
        Ok(())
    }

    #[tokio::test]
    async fn file_create_then_pwrite_writes_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/new.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &[1, 2, 3]).await?;

        assert_eq!(fs.read_file("/new.txt").await?.unwrap(), &[1, 2, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data = vec![0; chunk_size * 3];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let write_data: Vec<u8> = (0..20).collect();
        let start = chunk_size - 10;
        file.pwrite(start as u64, &write_data).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(&result[start..start + 20], &write_data[..]);
        assert_eq!(&result[0..start], &vec![0u8; start][..]);
        assert_eq!(
            &result[start + 20..],
            &vec![0u8; chunk_size * 3 - start - 20][..]
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_pwrite_roundtrip() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let initial: Vec<u8> = (0..(chunk_size * 2)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &initial).await?;

        let patches = [
            (0u64, vec![0xAAu8; 10]),
            (chunk_size as u64 - 5, vec![0xBB; 10]),
            (chunk_size as u64 * 2 - 1, vec![0xCC; 1]),
        ];

        for (offset, data) in &patches {
            file.pwrite(*offset, data).await?;
        }
        for (offset, expected) in &patches {
            assert_eq!(
                file.pread(*offset, expected.len() as u64).await?,
                expected.as_slice()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_preserves_order_and_inline_storage() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/batch-inline.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite_ranges(vec![
            WriteRange {
                offset: 0,
                data: b"abcdef".to_vec(),
            },
            WriteRange {
                offset: 2,
                data: b"ZZ".to_vec(),
            },
            WriteRange {
                offset: 6,
                data: b"!".to_vec(),
            },
        ])
        .await?;

        let ino = fs.resolve_path("/batch-inline.txt").await?.unwrap();
        assert_eq!(file.pread(0, 16).await?, b"abZZef!");
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(
            fs.get_storage_state(ino).await?,
            (STORAGE_INLINE, Some(b"abZZef!".to_vec()))
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_disjoint_inplace_writes_stay_inline() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let initial: Vec<u8> = (0..128).collect();
        let (_, file) = fs
            .create_file("/batch-inplace.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &initial).await?;

        file.pwrite_ranges(vec![
            WriteRange {
                offset: 8,
                data: b"ABCD".to_vec(),
            },
            WriteRange {
                offset: 64,
                data: b"WXYZ".to_vec(),
            },
        ])
        .await?;

        let mut expected = initial;
        expected[8..12].copy_from_slice(b"ABCD");
        expected[64..68].copy_from_slice(b"WXYZ");

        let ino = fs.resolve_path("/batch-inplace.bin").await?.unwrap();
        assert_eq!(file.pread(0, expected.len() as u64).await?, expected);
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_sparse_write_transitions_to_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/batch-sparse.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite_ranges(vec![
            WriteRange {
                offset: 0,
                data: b"head".to_vec(),
            },
            WriteRange {
                offset: fs.chunk_size() as u64 + 4,
                data: b"tail".to_vec(),
            },
        ])
        .await?;

        let ino = fs.resolve_path("/batch-sparse.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 2);

        let mut expected = b"head".to_vec();
        expected.resize(fs.chunk_size() + 4, 0);
        expected.extend_from_slice(b"tail");
        assert_eq!(file.pread(0, expected.len() as u64).await?, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_batched_drains_explicitly() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/batched.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![
            WriteRange {
                offset: 0,
                data: b"hello".to_vec(),
            },
            WriteRange {
                offset: 5,
                data: b" world".to_vec(),
            },
        ])
        .await?;

        let flushed_stats = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            flushed_stats.size, 11,
            "metadata reads should drain pending batched writes before reporting size"
        );

        file.drain_writes().await?;
        assert_eq!(file.pread(0, 32).await?, b"hello world");

        Ok(())
    }

    #[tokio::test]
    async fn test_setattr_after_batched_write_preserves_explicit_times() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/setattr-after-write.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Buffered write stays in the overlay (long timer, no drain).
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"deferred body".to_vec(),
        }])
        .await?;

        // Explicit setattr (the kernel's writeback mtime update) lands while
        // the data is still pending. No drain happens here by default.
        let explicit_secs = 1_234_567_890;
        let explicit_nsec = 42;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Omit,
            TimeChange::Set(explicit_secs, explicit_nsec),
        )
        .await?;

        // The deferred commit must NOT re-stamp mtime/ctime over the explicit
        // value the setattr just wrote.
        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            after.mtime, explicit_secs,
            "explicit mtime must survive the deferred data commit"
        );
        assert_eq!(
            after.mtime_nsec, explicit_nsec,
            "explicit mtime_nsec must survive the deferred data commit"
        );
        assert_eq!(after.size, 13);
        assert_eq!(file.pread(0, 32).await?, b"deferred body");

        Ok(())
    }

    #[tokio::test]
    async fn test_write_after_setattr_restamps_times_on_commit() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/write-after-setattr.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"first".to_vec(),
        }])
        .await?;

        let stale_secs = 1_111_111_111;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Omit,
            TimeChange::Set(stale_secs, 0),
        )
        .await?;

        // A write AFTER the setattr means the file changed again: the commit
        // must stamp fresh mtime/ctime, not preserve the stale explicit value.
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 5,
            data: b" second".to_vec(),
        }])
        .await?;

        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert!(
            after.mtime > stale_secs,
            "a write after the explicit setattr must bump mtime again (got {}, explicit was {})",
            after.mtime,
            stale_secs
        );
        assert_eq!(file.pread(0, 32).await?, b"first second");

        Ok(())
    }

    #[tokio::test]
    async fn test_utimens_with_pending_writes_is_visible_and_committed_with_data() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/stash-times.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Buffered write stays in the overlay (long timer, no drain).
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"stash body".to_vec(),
        }])
        .await?;

        // The explicit setattr is stashed in the pending entry instead of
        // paying its own SQLite transaction.
        let explicit_secs = 1_999_999_999;
        let explicit_nsec: u32 = 7;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Set(11, 13),
            TimeChange::Set(explicit_secs, explicit_nsec),
        )
        .await?;

        // Visible immediately, before any drain commits the row UPDATE.
        let before = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            before.mtime, explicit_secs,
            "stashed mtime must be visible before the drain commits it"
        );
        assert_eq!(before.mtime_nsec, explicit_nsec);
        assert_eq!(before.atime, 11);
        assert_eq!(before.atime_nsec, 13);
        assert_eq!(before.size, 10, "pending data size must still be merged");

        // The drain commits the data and the stashed times in one transaction.
        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            after.mtime, explicit_secs,
            "explicit mtime must survive the deferred data commit"
        );
        assert_eq!(after.mtime_nsec, explicit_nsec);
        assert_eq!(after.atime, 11);
        assert_eq!(after.atime_nsec, 13);
        assert_eq!(after.size, 10);
        assert_eq!(file.pread(0, 32).await?, b"stash body");

        Ok(())
    }

    #[tokio::test]
    async fn test_write_after_stashed_utimens_restamps_mtime_keeps_atime() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/stash-then-write.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"first".to_vec(),
        }])
        .await?;

        let stale_secs = 1_222_222_222;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Set(33, 44),
            TimeChange::Set(stale_secs, 0),
        )
        .await?;

        // A write AFTER the stashed setattr means the file changed again: the
        // commit must stamp fresh mtime/ctime. The explicitly-set atime is not
        // affected by writes and must survive.
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 5,
            data: b" second".to_vec(),
        }])
        .await?;

        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert!(
            after.mtime > stale_secs,
            "a write after the stashed setattr must bump mtime again (got {}, explicit was {})",
            after.mtime,
            stale_secs
        );
        assert_eq!(after.atime, 33, "explicit atime must survive a later write");
        assert_eq!(after.atime_nsec, 44);
        assert_eq!(file.pread(0, 32).await?, b"first second");

        Ok(())
    }

    // Build a batcher with an explicit config so the test is independent of the
    // process-global AGENTFS_BATCH_* env vars (which other tests mutate
    // concurrently). Reuses `fs`'s pool/attr cache so commits hit real inodes.
    fn test_batcher(
        fs: &AgentFS,
        batch_ms_secs: u64,
        batch_bytes: usize,
        batch_global_bytes: usize,
    ) -> Arc<AgentFSWriteBatcher> {
        let config = BatcherConfig {
            window: std::time::Duration::from_secs(batch_ms_secs),
            inode_bytes: batch_bytes,
            global_bytes: batch_global_bytes,
            ..BatcherConfig::default()
        };
        Arc::new(AgentFSWriteBatcher::from_config(
            fs.pool.clone(),
            fs.chunk_size,
            fs.inline_threshold,
            {
                let attr_cache = Arc::clone(&fs.attr_cache);
                Arc::new(move |ino| attr_cache.remove(ino))
            },
            &config,
        ))
    }

    #[tokio::test]
    async fn test_batcher_bytes_trigger_restamps_after_explicit_times() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (stats, _file) = fs
            .create_file("/bytes-trigger-times.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Long timer and huge global cap make the per-inode byte cap the only
        // reachable synchronous drain trigger in this test.
        let batcher = test_batcher(&fs, 600, 8, 1 << 30);

        batcher
            .enqueue_for_test(
                stats.ino,
                vec![WriteRange {
                    offset: 0,
                    data: b"abcd".to_vec(),
                }],
            )
            .await?;
        assert!(
            batcher.has_pending(stats.ino),
            "below the per-inode byte cap, the write must stay pending"
        );

        let explicit_secs = 1_345_678_901;
        let explicit_nsec = 123;
        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode SET mtime = ?, mtime_nsec = ?, ctime = ?, ctime_nsec = ? WHERE ino = ?",
            (
                explicit_secs,
                explicit_nsec,
                explicit_secs,
                explicit_nsec,
                stats.ino,
            ),
        )
        .await?;
        batcher.mark_times_explicit(stats.ino);

        batcher
            .enqueue_for_test(
                stats.ino,
                vec![WriteRange {
                    offset: 4,
                    data: b"efgh".to_vec(),
                }],
            )
            .await?;
        assert!(
            !batcher.has_pending(stats.ino),
            "crossing the per-inode byte cap must drain this inode"
        );

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(after.size, 8);
        assert!(
            after.mtime > explicit_secs,
            "the second write crosses the Bytes cap after the explicit setattr, \
             so the drain must stamp a fresh mtime (got {}, explicit was {})",
            after.mtime,
            explicit_secs
        );
        assert_eq!(
            fs.read_file("/bytes-trigger-times.txt").await?.unwrap(),
            b"abcdefgh"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_batcher_global_cap_triggers_full_drain_and_tracks_total() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (sa, _fa) = fs.create_file("/a.bin", DEFAULT_FILE_MODE, 0, 0).await?;
        let (sb, _fb) = fs.create_file("/b.bin", DEFAULT_FILE_MODE, 0, 0).await?;

        // 10-minute timer and huge per-inode trigger so the ONLY drain path is
        // the 64-byte global cross-inode cap.
        let batcher = test_batcher(&fs, 600, 1 << 20, 64);

        // Write below the cap to inode A: stays pending.
        batcher
            .enqueue_for_test(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'x'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.total_pending_bytes(),
            50,
            "write below the global cap must remain in the overlay"
        );

        // Truncating into the pending range shrinks the tracked total.
        batcher.truncate_pending(sa.ino, 20);
        assert_eq!(
            batcher.total_pending_bytes(),
            20,
            "truncate_pending must shrink the running total to the kept prefix"
        );

        // Write to inode B crosses the cap (20 + 50 >= 64): a full batched drain
        // commits every pending inode and resets the running total to zero.
        batcher
            .enqueue_for_test(
                sb.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'y'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.total_pending_bytes(),
            0,
            "crossing the global cap must drain all pending inodes"
        );

        // Committed data is intact and reflects the truncate.
        assert_eq!(fs.read_file("/a.bin").await?.unwrap(), vec![b'x'; 20]);
        assert_eq!(fs.read_file("/b.bin").await?.unwrap(), vec![b'y'; 50]);
        Ok(())
    }

    #[tokio::test]
    async fn test_batcher_discard_pending_updates_total() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (sa, _fa) = fs.create_file("/c.bin", DEFAULT_FILE_MODE, 0, 0).await?;

        // No timer/bytes/global drain: writes accumulate so we can observe the
        // total before discarding.
        let batcher = test_batcher(&fs, 600, 1 << 20, 1 << 30);
        batcher
            .enqueue_for_test(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'z'; 100],
                }],
            )
            .await?;
        assert_eq!(batcher.total_pending_bytes(), 100);

        batcher.discard_pending(sa.ino);
        assert_eq!(
            batcher.total_pending_bytes(),
            0,
            "discard_pending must subtract the discarded inode's bytes"
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_to_zero() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(0).await?;

        assert!(fs.read_file("/test.txt").await?.unwrap().is_empty());
        assert_eq!(fs.stat("/test.txt").await?.unwrap().size, 0);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_smaller_within_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(50).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 50);
        assert_eq!(result, &data[..50]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_across_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        let new_size = chunk_size + chunk_size / 2;
        file.truncate(new_size as u64).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), new_size);
        assert_eq!(result, &data[..new_size]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(100).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 100);
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[..50], &data[..]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_nonexistent_open_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = fs.open("/nonexistent.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_at_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(chunk_size as u64).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), chunk_size);
        assert_eq!(result, &data[..chunk_size]);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_same_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = b"hello world";
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;
        rename_path_via_trait(&fs, "/old.txt", "/new.txt").await?;

        assert!(fs.stat("/old.txt").await?.is_none());
        assert_eq!(fs.read_file("/new.txt").await?.unwrap(), data);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_to_different_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/subdir", 0, 0).await?;
        let data = b"test data";
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;
        rename_path_via_trait(&fs, "/file.txt", "/subdir/file.txt").await?;

        assert!(fs.stat("/file.txt").await?.is_none());
        assert_eq!(fs.read_file("/subdir/file.txt").await?.unwrap(), data);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/src.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"source").await?;
        let (_, file) = fs.create_file("/dst.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"destination").await?;
        rename_path_via_trait(&fs, "/src.txt", "/dst.txt").await?;

        assert!(fs.stat("/src.txt").await?.is_none());
        assert_eq!(fs.read_file("/dst.txt").await?.unwrap(), b"source");
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/olddir", 0, 0).await?;
        let (_, file) = fs
            .create_file("/olddir/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;
        rename_path_via_trait(&fs, "/olddir", "/newdir").await?;

        assert!(fs.stat("/olddir").await?.is_none());
        assert!(fs.stat("/newdir").await?.is_some());
        assert_eq!(fs.read_file("/newdir/file.txt").await?.unwrap(), b"content");
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory_into_own_subtree_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/parent", 0, 0).await?;
        fs.mkdir("/parent/child", 0, 0).await?;

        let parent_ino = fs.stat("/parent").await?.unwrap().ino;
        let child_ino = fs.stat("/parent/child").await?.unwrap().ino;
        let root_before = fs.readdir(ROOT_INO).await?.unwrap();
        let parent_before = fs.readdir(parent_ino).await?.unwrap();
        let child_before = fs.readdir(child_ino).await?.unwrap();

        let result = rename_path_via_trait(&fs, "/parent", "/parent/child/parent").await;

        assert!(matches!(result, Err(Error::Fs(FsError::InvalidRename))));
        assert_eq!(fs.readdir(ROOT_INO).await?.unwrap(), root_before);
        assert_eq!(fs.readdir(parent_ino).await?.unwrap(), parent_before);
        assert_eq!(fs.readdir(child_ino).await?.unwrap(), child_before);
        assert!(fs.stat("/parent").await?.is_some());
        assert!(fs.stat("/parent/child").await?.is_some());
        assert!(fs.stat("/parent/child/parent").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_corrupt_dentry_cycle_returns_invalid_path() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/src", 0, 0).await?;
        fs.mkdir("/cycle-a", 0, 0).await?;
        fs.mkdir("/cycle-a/cycle-b", 0, 0).await?;

        let cycle_a = fs.stat("/cycle-a").await?.unwrap().ino;
        let cycle_b = fs.stat("/cycle-a/cycle-b").await?.unwrap().ino;
        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_dentry SET parent_ino = ? WHERE ino = ?",
            (cycle_b, cycle_a),
        )
        .await?;
        fs.invalidate_dentry(ROOT_INO, "cycle-a");
        fs.invalidate_dentry(cycle_a, "cycle-b");

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            FileSystem::rename(&fs, ROOT_INO, "src", cycle_a, "moved"),
        )
        .await
        .expect("corrupt dentry cycles must error instead of looping forever");

        assert!(matches!(result, Err(Error::Fs(FsError::InvalidPath))));
        assert!(
            FileSystem::lookup(&fs, ROOT_INO, "src").await?.is_some(),
            "failed guarded rename must preserve the source directory"
        );
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = rename_path_via_trait(&fs, "/", "/newroot").await;
        assert!(matches!(result, Err(Error::Fs(FsError::RootOperation))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_to_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        let result = rename_path_via_trait(&fs, "/file.txt", "/").await;
        assert!(matches!(result, Err(Error::Fs(FsError::RootOperation))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_nonexistent_source_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = rename_path_via_trait(&fs, "/nonexistent.txt", "/new.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_overwrite_nonempty_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/src", 0, 0).await?;
        fs.mkdir("/dst", 0, 0).await?;
        let (_, file) = fs
            .create_file("/dst/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;

        let result = rename_path_via_trait(&fs, "/src", "/dst").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotEmpty))));
        assert!(fs.stat("/src").await?.is_some());
        assert!(fs.stat("/dst").await?.is_some());
        assert!(fs.stat("/dst/file.txt").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_to_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        fs.mkdir("/dir", 0, 0).await?;

        let result = rename_path_via_trait(&fs, "/file.txt", "/dir").await;
        assert!(matches!(result, Err(Error::Fs(FsError::IsADirectory))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory_to_file_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/dir", 0, 0).await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;

        let result = rename_path_via_trait(&fs, "/dir", "/file.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotADirectory))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_updates_ctime() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        let stats_before = fs.stat("/old.txt").await?.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        rename_path_via_trait(&fs, "/old.txt", "/new.txt").await?;

        let stats_after = fs.stat("/new.txt").await?.unwrap();
        assert!(stats_after.ctime >= stats_before.ctime);
        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_regular_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file with default permissions
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"content").await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        let ino = stats.ino;
        assert_eq!(
            stats.mode & 0o7777,
            0o644,
            "Default file mode should be 0o644"
        );

        // Change to executable
        fs.chmod(ino, 0o755).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o755,
            "Mode should be 0o755 after chmod"
        );
        assert!(stats.is_file(), "Should still be a regular file");

        // Change to read-only
        fs.chmod(ino, 0o444).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o444,
            "Mode should be 0o444 after chmod"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_preserves_file_type() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a regular file
        let (file_stats, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"content").await?;
        fs.chmod(file_stats.ino, 0o755).await?;
        let stats = fs.stat("/file.txt").await?.unwrap();
        assert!(stats.is_file(), "Should remain a regular file after chmod");

        // Create a directory
        fs.mkdir("/dir", 0, 0).await?;
        let dir_stats = fs.stat("/dir").await?.unwrap();
        fs.chmod(dir_stats.ino, 0o700).await?;
        let stats = fs.stat("/dir").await?.unwrap();
        assert!(
            stats.is_directory(),
            "Should remain a directory after chmod"
        );
        assert_eq!(stats.mode & 0o7777, 0o700, "Directory mode should be 0o700");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_nonexistent_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Use a non-existent inode
        let result = fs.chmod(999999, 0o755).await;
        assert!(result.is_err(), "chmod on nonexistent inode should fail");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_symlink() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create target and symlink
        let (_, file) = fs
            .create_file("/target.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;
        FileSystem::symlink(&fs, ROOT_INO, "link.txt", "/target.txt", 0, 0).await?;
        let link_stats = FileSystem::lookup(&fs, ROOT_INO, "link.txt")
            .await?
            .unwrap();

        // chmod the symlink (should work on the symlink inode)
        fs.chmod(link_stats.ino, 0o755).await?;

        let stats = FileSystem::lookup(&fs, ROOT_INO, "link.txt")
            .await?
            .unwrap();
        assert!(stats.is_symlink(), "Should still be a symlink");

        Ok(())
    }

    // ==================== Tier Four: Overlay Read-After-Write ====================
    //
    // These exercise the Tier 4 invariant that `pread` / `getattr` /
    // `truncate` reflect pending batched writes BEFORE the SQLite drain
    // commits them — i.e. the per-fd write-then-read story works without
    // forcing a synchronous SQLite transaction on every read.

    #[tokio::test]
    async fn pread_after_uncommitted_pwrite_sees_pending() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/overlay.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello world").await?;
        // No fsync — Tier 4 says the same fd must see its own writes via
        // the in-memory overlay, regardless of whether SQLite has them yet.
        assert_eq!(file.pread(0, 11).await?, b"hello world");
        assert_eq!(file.pread(6, 5).await?, b"world");
        Ok(())
    }

    #[tokio::test]
    async fn pread_after_uncommitted_pwrite_partial_overlap() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/over.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"AAAAAAAAAA").await?;
        file.fsync().await?;
        file.pwrite(4, b"BBB").await?;
        // Read spans SQLite-resident (A) and pending (B) regions.
        assert_eq!(file.pread(2, 6).await?, b"AABBBA");
        Ok(())
    }

    #[tokio::test]
    async fn pread_in_unwritten_region_returns_sqlite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/hole.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &[0xCDu8; 64]).await?;
        file.fsync().await?;
        file.pwrite(80, b"tail").await?;
        // Read [16, 32) — entirely SQLite, no pending overlap.
        assert_eq!(file.pread(16, 16).await?, vec![0xCDu8; 16]);
        Ok(())
    }

    #[tokio::test]
    async fn truncate_drops_pending_beyond_new_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/trunc.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"abcdef").await?;
        file.truncate(3).await?;
        assert_eq!(file.pread(0, 16).await?, b"abc");
        let attrs = FileSystem::getattr(&fs, fs.resolve_path("/trunc.txt").await?.unwrap())
            .await?
            .unwrap();
        assert_eq!(attrs.size, 3);
        Ok(())
    }

    #[tokio::test]
    async fn truncate_clips_range_spanning_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/clip.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(2, b"PPPPPP").await?;
        // pending occupies [2, 8). Truncate to 5 should keep [2, 5).
        file.truncate(5).await?;
        assert_eq!(file.pread(0, 16).await?, vec![0, 0, b'P', b'P', b'P']);
        Ok(())
    }

    #[tokio::test]
    async fn stat_coherent_before_drain() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (created, file) = fs.create_file("/grow.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        let pre = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(pre.size, 0);
        file.pwrite(0, b"abcdefghij").await?;
        assert!(
            fs.write_batcher
                .as_ref()
                .is_some_and(|batcher| batcher.has_pending(created.ino)),
            "long-window write should still be pending before stat"
        );
        let conn = fs.pool.get_connection().await?;
        let sqlite = store::getattr(&conn, created.ino)
            .await?
            .expect("file should exist");
        assert_eq!(
            sqlite.size, 0,
            "SQLite row should not be drained before the stat coherence check"
        );
        drop(conn);
        let post = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(post.size, 10);
        Ok(())
    }

    #[tokio::test]
    async fn versioned_cache_fill_skips_stale_attrs_after_drain() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (created, file) = fs
            .create_file("/stale-cache.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite(0, b"pending").await?;

        let conn = fs.pool.get_connection().await?;
        let generation = fs.pending_generation(created.ino);
        let mut stale_stats = store::getattr(&conn, created.ino)
            .await?
            .expect("file should exist");
        self::AgentFS::merge_pending_view(&fs, created.ino, Some(&mut stale_stats));
        drop(conn);

        file.drain_writes().await?;
        fs.cache_attr_if_pending_generation(stale_stats, generation);

        assert!(
            cached_attr(&fs, created.ino).is_none(),
            "a cache fill captured before a racing drain must be skipped"
        );

        let current = FileSystem::getattr(&fs, created.ino)
            .await?
            .expect("file should still exist");
        assert_eq!(current.size, 7);
        assert_eq!(file.pread(0, 16).await?, b"pending");
        Ok(())
    }

    #[tokio::test]
    async fn retired_generations_prune_without_cache_fill_aba() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (created, file) = fs
            .create_file("/pruned-generation.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite(0, b"pending").await?;

        let conn = fs.pool.get_connection().await?;
        let generation = fs.pending_generation(created.ino);
        let mut stale_stats = store::getattr(&conn, created.ino)
            .await?
            .expect("file should exist");
        self::AgentFS::merge_pending_view(&fs, created.ino, Some(&mut stale_stats));
        drop(conn);

        file.drain_writes().await?;
        let batcher = fs
            .write_batcher
            .as_ref()
            .expect("long-window test config enables the batcher");
        assert!(
            batcher.retired_generation_contains(created.ino),
            "draining a pending entry should retire its generation before pruning"
        );

        for idx in 0..(batcher::MAX_RETIRED_GENERATIONS + 8) {
            let path = format!("/retired-{idx}.txt");
            let (stats, _file) = fs.create_file(&path, DEFAULT_FILE_MODE, 0, 0).await?;
            batcher
                .enqueue_for_test(
                    stats.ino,
                    vec![WriteRange {
                        offset: 0,
                        data: vec![b'x'],
                    }],
                )
                .await?;
            batcher.discard_pending(stats.ino);
        }

        assert!(
            batcher.retired_generation_count() <= batcher::MAX_RETIRED_GENERATIONS,
            "retired generations must stay bounded under inode churn"
        );
        assert!(
            !batcher.retired_generation_contains(created.ino),
            "the oldest retired generation should have been pruned by the sweep"
        );

        fs.cache_attr_if_pending_generation(stale_stats, generation);
        assert!(
            cached_attr(&fs, created.ino).is_none(),
            "a cache fill that observed the pre-drain generation must still be skipped after pruning"
        );

        let current = FileSystem::getattr(&fs, created.ino)
            .await?
            .expect("file should still exist");
        assert_eq!(current.size, 7);
        assert_eq!(file.pread(0, 16).await?, b"pending");
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_writers_overlay_merge() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, fh_a) = fs
            .create_file("/multi.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = fs.resolve_path("/multi.txt").await?.unwrap();
        let fh_b = fs.open("/multi.txt").await?;
        fh_a.pwrite(0, b"AAAA").await?;
        fh_b.pwrite(4, b"BBBB").await?;
        // Either fd should see both writes merged via the overlay.
        assert_eq!(fh_a.pread(0, 8).await?, b"AAAABBBB");
        assert_eq!(fh_b.pread(0, 8).await?, b"AAAABBBB");
        // And getattr reflects the combined size.
        let attrs = FileSystem::getattr(&fs, ino).await?.unwrap();
        assert_eq!(attrs.size, 8);
        Ok(())
    }

    #[tokio::test]
    async fn unlink_during_pending_writes_no_orphan() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/doomed.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"these bytes never reach SQLite").await?;
        // Unlink before any drain. Tier 4 hooks discard_pending here.
        fs.remove("/doomed.txt").await?;
        // Force a batched drain. If pending was not discarded, the drain
        // would hit NotFound while looking up fs_inode for the
        // unlinked ino. The drain must therefore succeed.
        fs.drain_all().await?;
        // And the row truly is gone.
        assert!(fs.stat("/doomed.txt").await?.is_none());
        let conn = fs.pool.get_connection().await?;
        let count: i64 = {
            let mut rows = conn
                .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (created.ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(count, 0, "no orphan fs_data rows for unlinked ino");
        Ok(())
    }

    #[tokio::test]
    async fn fsync_drains_overlay_to_sqlite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/durable.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"persist me").await?;
        // Before fsync, the bytes are in the overlay; get_chunk_count drains
        // them as part of the test helper (Tier 4 sync helper change).
        // After fsync, the chunk count should be observable without any
        // helper drain prelude.
        file.fsync().await?;
        let conn = fs.pool.get_connection().await?;
        let count: i64 = {
            let mut rows = conn
                .query("SELECT size FROM fs_inode WHERE ino = ?", (created.ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(count, 10, "fsync committed pending size to fs_inode");
        Ok(())
    }

    /// Spec acceptance criterion for Tier 4:
    /// "`agentfs_batcher_drains_explicit / agentfs_batcher_enqueues` ratio
    /// drops to <0.2 (vs ~1.0 today) — confirms read path no longer triggers
    /// Explicit drains."
    ///
    /// We simulate a read-after-write workload (write, read, write, read, ...)
    /// and assert that the SDK does NOT call drain_inode_writes
    /// (Explicit drain) on every read. With Tier 4 the read path peeks the
    /// overlay; with Tier 3 each read forces drain → ratio ≈ 1.0.
    #[tokio::test]
    async fn tier_four_drains_explicit_to_enqueues_ratio_under_0_2() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/ratio.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        let pre = crate::profiling::snapshot();
        let pre_enq = pre.counter("agentfs_batcher_enqueues");
        let pre_explicit = pre.counter("agentfs_batcher_drains_explicit");

        // 200 write-then-read cycles, no intervening fsync. Tier 3 would
        // drain Explicit on every read; Tier 4 must not.
        for i in 0..200u64 {
            file.pwrite(i * 4, b"abcd").await?;
            let _ = file.pread(i * 4, 4).await?;
        }

        let post = crate::profiling::snapshot();
        let enq = post.counter("agentfs_batcher_enqueues") - pre_enq;
        let explicit = post.counter("agentfs_batcher_drains_explicit") - pre_explicit;
        assert!(enq >= 200, "expected ≥200 enqueues, got {enq}");
        let ratio = explicit as f64 / enq.max(1) as f64;
        assert!(
            ratio < 0.2,
            "Tier 4 acceptance: drains_explicit/enqueues should be <0.2; \
             got {explicit}/{enq} = {ratio:.3}"
        );
        Ok(())
    }

    /// Spec escape-hatch verification: with the overlay disabled, the SDK
    /// reverts to Tier 3 drain-on-write semantics. `pwrite` should commit
    /// straight to SQLite (no batcher enqueue), and `pread` should see the
    /// value without ever consulting `peek_pending`. This locks in the kill
    /// switch the spec's risk table called for.
    #[tokio::test]
    async fn overlay_reads_flag_off_falls_back_to_drain_on_write() -> Result<()> {
        let (mut fs, _dir) = create_test_fs().await?;
        fs.overlay_reads = false;
        let (_, file) = fs
            .create_file("/escape.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite(0, b"hello world").await?;
        // Per-inode check rather than the global enqueue counter: parallel
        // tests share the profiling globals, so counter deltas race.
        let escape_ino = fs.resolve_path("/escape.bin").await?.unwrap();
        if let Some(batcher) = &fs.write_batcher {
            assert!(
                !batcher.has_pending(escape_ino),
                "with overlay_reads=false, pwrite must not enqueue"
            );
        }
        let got = file.pread(0, 11).await?;
        assert_eq!(&got, b"hello world");

        // And the file is durably in SQLite without an explicit fsync —
        // the Tier 3 contract.
        let ino = fs.resolve_path("/escape.bin").await?.unwrap();
        let conn = fs.pool.get_connection().await?;
        let size: i64 = {
            let mut rows = conn
                .query("SELECT size FROM fs_inode WHERE ino = ?", (ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(
            size, 11,
            "overlay_reads=false → SQLite has full size after pwrite"
        );
        Ok(())
    }
