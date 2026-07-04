    use super::*;
    use crate::fs::HostFS;
    use crate::fs::{FsError, TimeChange};
    use crate::DEFAULT_FILE_MODE;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    async fn create_test_overlay() -> Result<(OverlayFS, tempfile::TempDir, tempfile::TempDir)> {
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("base.txt"), b"base content")?;
        std::fs::create_dir(base_dir.path().join("subdir"))?;
        std::fs::write(base_dir.path().join("subdir/nested.txt"), b"nested")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        Ok((overlay, base_dir, delta_dir))
    }

    #[tokio::test]
    async fn test_overlay_lookup_base() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup file from base
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_in_delta() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Create file in delta
        let (stats, file) = overlay
            .create_file(ROOT_INO, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify it exists
        let lookup_stats = overlay.lookup(ROOT_INO, "new.txt").await?.unwrap();
        assert_eq!(lookup_stats.ino, stats.ino);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_whiteout() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // File exists initially
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_some());

        // Delete it
        overlay.unlink(ROOT_INO, "base.txt").await?;

        // File should be gone
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        // Open and write to it (should trigger copy-up)
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified content").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify reading through overlay returns modified content
        let read_back = file.pread(0, 100).await?;
        assert_eq!(
            read_back, b"modified content",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_read_only_base_open_does_not_copy_up() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDONLY).await?;

        assert_eq!(file.pread(0, 100).await?, b"base content");
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_origin").await?,
            0,
            "read-only open of a base file should not create origin mappings"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            0,
            "read-only open of a base file should not copy file bytes into delta"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_keep_cache_only_for_read_only_base_files() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let granted = overlay
            .keep_cache_for_read_open(stats.ino, libc::O_RDONLY)
            .await?;
        assert!(
            granted.is_some(),
            "read-only base files are eligible for FOPEN_KEEP_CACHE"
        );
        assert_eq!(
            granted.map(|s| s.size),
            Some(stats.size),
            "keep-cache grant must carry the stats it was decided on"
        );
        assert!(
            overlay
                .keep_cache_for_read_open(stats.ino, libc::O_RDWR)
                .await?
                .is_none(),
            "writable opens must not keep the base page cache"
        );

        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified content").await?;
        assert!(
            overlay
                .keep_cache_for_read_open(stats.ino, libc::O_RDONLY)
                .await?
                .is_some(),
            "delta-backed files stay keep-cache eligible; staleness is the \
             adapter fingerprint guard's job"
        );
        // The fingerprint inputs must have moved across the copy-up + write so
        // the adapter rejects any pages cached against the base version.
        let after = overlay.getattr(stats.ino).await?.unwrap();
        assert!(
            (after.size, after.mtime, after.mtime_nsec, after.ctime)
                != (stats.size, stats.mtime, stats.mtime_nsec, stats.ctime),
            "copy-up + write must change the stats fingerprint"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_inode_stability() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file and record its inode
        let stats_before = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats_before.ino;

        // Open triggers copy-up
        let file = overlay.open(stats_before.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified").await?;

        // Lookup again - inode should be the same
        let stats_after = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_origin_mapping_rejects_wrong_path_base_inode() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let subdir = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        let nested = overlay.lookup(subdir.ino, "nested.txt").await?.unwrap();
        let nested_base_ino = overlay.get_inode_info(nested.ino).unwrap().underlying_ino;

        let (delta_stats, _file) = <AgentFS as FileSystem>::create_file(
            overlay.delta(),
            ROOT_INO,
            "base.txt",
            DEFAULT_FILE_MODE,
            0,
            0,
        )
        .await?;
        overlay
            .add_origin_mapping(delta_stats.ino, nested_base_ino)
            .await?;

        let resolved = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_ne!(
            resolved.ino, nested.ino,
            "origin mapping must not reuse a live base inode for a different path"
        );
        assert_eq!(
            overlay.get_inode_info(resolved.ino).unwrap().path,
            "/base.txt"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_single_byte_write_stores_one_chunk() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 3 + 17, 0x21);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 123;
        file.pwrite(write_offset, b"Z").await?;

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            1,
            "single-byte partial-origin write should materialize one chunk"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            1,
            "single-byte partial-origin write should record one chunk override"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT SUM(LENGTH(data)) FROM fs_data").await?,
            chunk_size as i64,
            "materialized chunk should be bounded to the configured chunk size"
        );

        let read_back = file.pread(write_offset - 2, 5).await?;
        let mut expected =
            base_content[write_offset as usize - 2..write_offset as usize + 3].to_vec();
        expected[2] = b'Z';
        assert_eq!(read_back, expected);
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "base file should remain unchanged"
        );

        Ok(())
    }

    #[tokio::test]
    async fn overlay_partial_origin_store_characterization() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 17, 0x2a);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;

        let cross_chunk_offset = chunk_size as u64 - 2;
        file.pwrite(cross_chunk_offset, b"STORE").await?;
        expected[cross_chunk_offset as usize..cross_chunk_offset as usize + 5]
            .copy_from_slice(b"STORE");

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            2,
            "shared store write should materialize only the touched chunks"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            2,
            "shared store hook should mark exactly the touched chunks as overrides"
        );
        assert_eq!(
            file.pread(chunk_size as u64 - 4, 10).await?,
            expected[chunk_size - 4..chunk_size + 6],
            "partial-origin reads should merge store-owned chunks with base fallback bytes"
        );

        file.truncate(chunk_size as u64 + 1).await?;
        expected.truncate(chunk_size + 1);
        assert_eq!(file.fstat().await?.size, (chunk_size + 1) as i64);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            2,
            "truncate within an overridden chunk should keep in-range override markers"
        );
        assert_eq!(
            file.pread(chunk_size as u64 - 4, 8).await?,
            expected[chunk_size - 4..],
            "truncate through the shared store should preserve in-range override bytes"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            patterned_bytes(chunk_size * 2 + 17, 0x2a),
            "partial-origin store writes and truncates must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_policy_off_uses_whole_copy_up() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 2 + 11, 0x17);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::Off),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 3, b"X").await?;

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "explicit off policy must keep whole-file copy-up semantics"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "whole-file copy-up must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_policy_auto_threshold() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let threshold = (chunk_size * 2) as u64;
        let small_content = patterned_bytes(chunk_size + 31, 0x05);
        let large_content = patterned_bytes(chunk_size * 2 + 31, 0x55);
        std::fs::write(base_dir.path().join("small.bin"), &small_content)?;
        std::fs::write(base_dir.path().join("large.bin"), &large_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::Auto).with_threshold_bytes(threshold),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let small_stats = overlay.lookup(ROOT_INO, "small.bin").await?.unwrap();
        let small_file = overlay.open(small_stats.ino, libc::O_RDWR).await?;
        small_file.pwrite(3, b"s").await?;
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "auto policy should whole-copy files below the threshold"
        );

        let large_stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let large_file = overlay.open(large_stats.ino, libc::O_RDWR).await?;
        large_file.pwrite(chunk_size as u64 + 7, b"L").await?;
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "auto policy should use partial-origin at or above the threshold"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("small.bin"))?,
            small_content,
            "small-file write must not mutate the base file"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            large_content,
            "large-file partial-origin write must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_metadata_paths_do_not_mutate_base() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let base_path = base_dir.path().join("file.txt");
        std::fs::write(&base_path, b"metadata base")?;

        let base_meta_before = std::fs::metadata(&base_path)?;
        let base_mode_before = base_meta_before.permissions().mode() & 0o777;
        let base_modified_before = base_meta_before.modified()?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::On),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "file.txt").await?.unwrap();
        overlay.chmod(stats.ino, 0o600).await?;
        overlay
            .utimens(
                stats.ino,
                TimeChange::Set(123, 456),
                TimeChange::Set(789, 123),
            )
            .await?;

        let overlay_stats = overlay.getattr(stats.ino).await?.unwrap();
        assert_eq!(overlay_stats.mode & 0o777, 0o600);
        assert_eq!(overlay_stats.atime, 123);
        assert_eq!(overlay_stats.atime_nsec, 456);
        assert_eq!(overlay_stats.mtime, 789);
        assert_eq!(overlay_stats.mtime_nsec, 123);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "metadata-only paths should target partial-origin delta metadata"
        );

        let base_meta_after = std::fs::metadata(&base_path)?;
        assert_eq!(
            base_meta_after.permissions().mode() & 0o777,
            base_mode_before,
            "chmod through overlay must not mutate base permissions"
        );
        assert_eq!(
            base_meta_after.modified()?,
            base_modified_before,
            "utimens through overlay must not mutate base mtime"
        );
        assert_eq!(std::fs::read(&base_path)?, b"metadata base");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_reads_across_override_boundaries() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 32, 0x42);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 - 2;
        file.pwrite(write_offset, b"WXYZ").await?;
        expected[write_offset as usize..write_offset as usize + 4].copy_from_slice(b"WXYZ");

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            2,
            "cross-boundary write should materialize only the two touched chunks"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            2
        );

        let read_back = file.pread(chunk_size as u64 - 4, 8).await?;
        assert_eq!(
            read_back,
            expected[chunk_size - 4..chunk_size + 4],
            "read should merge delta-owned chunks with base fallback bytes"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_truncate_extend_does_not_reexpose_base_tail() -> Result<()>
    {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 2, 0x63);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.truncate((chunk_size + 5) as u64).await?;
        file.truncate((chunk_size + 12) as u64).await?;

        let after_extend = file.pread(chunk_size as u64 + 4, 8).await?;
        let mut expected = vec![base_content[chunk_size + 4]];
        expected.extend(std::iter::repeat_n(0u8, 7));
        assert_eq!(
            after_extend, expected,
            "extend after shrink should return zeros instead of base fallback past the shrink point"
        );
        assert_eq!(file.fstat().await?.size, (chunk_size + 12) as i64);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_open_truncates_base_file_mapping() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        std::fs::write(base_dir.path().join("large.bin"), b"base contents")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay
            .open(stats.ino, libc::O_RDWR | libc::O_TRUNC)
            .await?;
        assert_eq!(file.fstat().await?.size, 0);
        assert_eq!(file.pread(0, 32).await?, b"");
        assert_eq!(overlay.getattr(stats.ino).await?.unwrap().size, 0);
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            b"base contents",
            "O_TRUNC through the overlay must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_open_truncates_existing_partial_file() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        std::fs::write(base_dir.path().join("large.bin"), b"base contents")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(5, b"X").await?;
        assert_eq!(file.pread(0, 16).await?, b"base Xontents");

        let truncated = overlay
            .open(stats.ino, libc::O_RDWR | libc::O_TRUNC)
            .await?;
        assert_eq!(truncated.fstat().await?.size, 0);
        assert_eq!(truncated.pread(0, 32).await?, b"");
        assert_eq!(overlay.getattr(stats.ino).await?.unwrap().size, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_survives_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 9, 0x31);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 7;
        file.pwrite(write_offset, b"R").await?;
        file.fsync().await?;
        expected[write_offset as usize] = b'R';

        drop(file);
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = reopened.open(stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(chunk_size as u64 + 4, 8).await?,
            expected[chunk_size + 4..chunk_size + 12],
            "partial-origin reads must resolve persisted base_path after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_readdir_plus_survives_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 9, 0x41);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(4, b"Q").await?;
        file.fsync().await?;
        expected[4] = b'Q';
        drop(file);
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let entries = reopened.readdir_plus(ROOT_INO).await?.unwrap();
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == "large.bin")
            .expect("large.bin from readdir_plus");
        let file = reopened.open(entry.stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(0, 8).await?,
            expected[..8],
            "readdir_plus inode should open the partial-origin delta view after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_rename_keeps_live_mapping() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 16, 0x51);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(3, b"Z").await?;
        expected[3] = b'Z';
        drop(file);

        overlay
            .rename(ROOT_INO, "large.bin", ROOT_INO, "renamed.bin")
            .await?;
        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        let renamed = overlay.lookup(ROOT_INO, "renamed.bin").await?.unwrap();
        let file = overlay.open(renamed.ino, libc::O_RDONLY).await?;
        assert_eq!(file.pread(0, 8).await?, expected[..8]);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_base_drift() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 16, 0x71);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"Z").await?;
        drop(file);
        drop(overlay);

        std::fs::write(base_dir.path().join("large.bin"), b"changed base")?;

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        assert!(
            reopened.open(stats.ino, libc::O_RDONLY).await.is_err(),
            "partial-origin files should fail loudly when the base fallback changed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_base_drift_after_open() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_file = base_dir.path().join("large.bin");
        let base_content = patterned_bytes(chunk_size * 2, 0x37);
        std::fs::write(&base_file, &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64, b"X").await?;

        let read_handle = overlay.open(stats.ino, libc::O_RDONLY).await?;
        std::fs::write(&base_file, patterned_bytes(chunk_size * 2, 0x91))?;

        let err = read_handle.pread(0, 8).await.unwrap_err();
        assert!(
            err.to_string().contains("partial-origin base changed"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_same_size_base_drift() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 16, 0x73);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"Z").await?;
        drop(file);
        drop(overlay);

        std::thread::sleep(std::time::Duration::from_millis(10));
        let changed_same_size = patterned_bytes(base_content.len(), 0x74);
        std::fs::write(base_dir.path().join("large.bin"), changed_same_size)?;

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        assert!(
            reopened.open(stats.ino, libc::O_RDONLY).await.is_err(),
            "partial-origin files should fail loudly when same-size base fallback content changed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_main_db_snapshot_restore() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let restored_db_path = delta_dir.path().join("restored.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 33, 0x91);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 11;
        file.pwrite(write_offset, b"S").await?;
        file.fsync().await?;
        expected[write_offset as usize] = b'S';
        drop(file);
        drop(overlay);

        std::fs::copy(&db_path, &restored_db_path)?;

        let restored_delta = AgentFS::new(restored_db_path.to_str().unwrap()).await?;
        let restored_base = Arc::new(HostFS::new(base_dir.path())?);
        let restored = OverlayFS::new_with_partial_origin(restored_base, restored_delta, true);
        restored.init(base_dir.path().to_str().unwrap()).await?;

        let restored_stats = restored.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let restored_file = restored.open(restored_stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            restored_file.pread(chunk_size as u64 + 8, 8).await?,
            expected[chunk_size + 8..chunk_size + 16],
            "main-db snapshot restore should preserve partial-origin metadata and chunk overrides"
        );
        assert_eq!(
            scalar_i64(&restored, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1
        );
        assert_eq!(
            scalar_i64(&restored, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            1
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_unlink_cleans_metadata_and_whiteouts_base() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 19, 0xa1);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 1, b"U").await?;
        drop(file);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1
        );

        overlay.unlink(ROOT_INO, "large.bin").await?;

        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "unlink should not mutate the base file"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "last unlink should remove partial-origin rows"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            0,
            "last unlink should remove chunk override rows"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_origin").await?,
            0,
            "last unlink should remove origin rows"
        );

        Ok(())
    }

    #[tokio::test]
    async fn overlay_reap_hook_cleans_sidecars() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        std::fs::write(
            base_dir.path().join("src.bin"),
            patterned_bytes(chunk_size + 17, 0xd1),
        )?;
        std::fs::write(
            base_dir.path().join("dst.bin"),
            patterned_bytes(chunk_size + 19, 0xe1),
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        for (name, byte) in [("src.bin", b'S'), ("dst.bin", b'D')] {
            let stats = overlay.lookup(ROOT_INO, name).await?.unwrap();
            let file = overlay.open(stats.ino, libc::O_RDWR).await?;
            file.pwrite(3, &[byte]).await?;
            file.fsync().await?;
            drop(file);
        }
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            2,
            "both partially copied files should have sidecar metadata before rename-replace"
        );

        overlay
            .rename(ROOT_INO, "src.bin", ROOT_INO, "dst.bin")
            .await?;
        assert_no_orphan_sidecars(&overlay, "rename-replace").await?;

        let (deferred, deferred_file) = FileSystem::create_file(
            overlay.delta(),
            ROOT_INO,
            "manual-deferred.bin",
            DEFAULT_FILE_MODE,
            0,
            0,
        )
        .await?;
        deferred_file.pwrite(0, b"manual").await?;
        deferred_file.drain_writes().await?;
        insert_manual_sidecars(overlay.delta(), deferred.ino).await?;
        FileSystem::unlink(overlay.delta(), ROOT_INO, "manual-deferred.bin").await?;
        assert_eq!(deferred_file.fstat().await?.nlink, 0);
        drop(deferred_file);
        overlay.delta().process_deferred_reaps().await?;
        assert_no_orphan_sidecars(&overlay, "deferred-reap").await?;

        let crash_dir = tempdir()?;
        let crash_db = crash_dir.path().join("crash.db");
        let crash_db_path = crash_db.to_string_lossy().into_owned();
        {
            let agent = crate::AgentFS::open(
                crate::AgentFSOptions::with_path(crash_db_path.clone()).with_base(base_dir.path()),
            )
            .await?;
            let (stats, file) = FileSystem::create_file(
                &agent.fs,
                ROOT_INO,
                "manual-crash.bin",
                DEFAULT_FILE_MODE,
                0,
                0,
            )
            .await?;
            file.pwrite(0, b"crash").await?;
            file.drain_writes().await?;
            insert_manual_sidecars(&agent.fs, stats.ino).await?;
            FileSystem::unlink(&agent.fs, ROOT_INO, "manual-crash.bin").await?;
            std::mem::forget(file);
        }

        let reopened_agent = crate::AgentFS::open(
            crate::AgentFSOptions::with_path(crash_db_path).with_base(base_dir.path()),
        )
        .await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_agent.fs, true);
        reopened.load().await?;
        assert_no_orphan_sidecars(&reopened, "crash-sweep").await?;

        Ok(())
    }

    #[tokio::test]
    async fn overlay_sidecar_reap_hook_registers_once() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        assert_eq!(delta.reap_hook_count(), 0);

        assert!(
            delta.register_reap_hook(OverlayFS::sidecar_reap_hook()),
            "first constructor-time sidecar hook registration should be accepted"
        );
        assert_eq!(delta.reap_hook_count(), 1);

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new(base, delta);
        assert_eq!(
            overlay.delta().reap_hook_count(),
            1,
            "OverlayFS construction must not duplicate a pre-registered sidecar hook"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_hardlink_survives_source_unlink() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 21, 0xb1);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(5, b"H").await?;
        expected[5] = b'H';
        drop(file);

        overlay.link(stats.ino, ROOT_INO, "linked.bin").await?;
        let linked = overlay.lookup(ROOT_INO, "linked.bin").await?.unwrap();
        assert_eq!(linked.ino, stats.ino);
        assert_eq!(linked.nlink, 2);
        let linked_file = overlay.open(linked.ino, libc::O_RDONLY).await?;
        assert_eq!(linked_file.pread(0, 8).await?, expected[..8]);
        drop(linked_file);

        overlay.unlink(ROOT_INO, "large.bin").await?;
        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        let linked_after = overlay.lookup(ROOT_INO, "linked.bin").await?.unwrap();
        let linked_file = overlay.open(linked_after.ino, libc::O_RDONLY).await?;
        assert_eq!(
            linked_file.pread(0, 8).await?,
            expected[..8],
            "hardlink should retain merged partial-origin contents after source unlink"
        );
        assert_eq!(linked_file.fstat().await?.nlink, 1);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "partial-origin metadata should remain while a hardlink keeps the inode alive"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_renamed_file_readdir_plus_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 23, 0xc1);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(7, b"N").await?;
        file.fsync().await?;
        expected[7] = b'N';
        drop(file);

        overlay
            .rename(ROOT_INO, "large.bin", ROOT_INO, "renamed.bin")
            .await?;
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        assert!(reopened.lookup(ROOT_INO, "large.bin").await?.is_none());
        let entries = reopened.readdir_plus(ROOT_INO).await?.unwrap();
        let renamed = entries
            .into_iter()
            .find(|entry| entry.name == "renamed.bin")
            .expect("renamed.bin from readdir_plus");
        let file = reopened.open(renamed.stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(0, 10).await?,
            expected[..10],
            "renamed partial-origin file from readdir_plus should open after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_default_copy_up_still_copies_whole_base_file() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 3 + 17, 0x84);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, false);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 123, b"Z").await?;
        // Tier Four: pwrite is batched in the delta SDK now; flush so the
        // fs_data row count below reflects the committed copy-up chunks.
        file.fsync().await?;

        assert!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await? > 1,
            "default overlay open/write path should keep whole-file copy-up behavior"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "partial-origin metadata must stay opt-in"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_chmod() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats.ino;

        // chmod should trigger copy-up
        overlay.chmod(stats.ino, 0o755).await?;

        // Verify base file mode is UNCHANGED
        let base_meta = std::fs::metadata(base_dir.path().join("base.txt"))?;
        assert_ne!(
            base_meta.permissions().mode() & 0o777,
            0o755,
            "base file mode should be unchanged"
        );

        // Verify overlay returns new mode
        let stats_after = overlay.getattr(stats.ino).await?.unwrap();
        assert_eq!(
            stats_after.mode & 0o777,
            0o755,
            "overlay should return new mode"
        );

        // Inode should remain stable
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after chmod copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_truncate() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(stats.size, 12); // "base content"

        // Open and truncate (triggers copy-up via open)
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.truncate(5).await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify overlay returns truncated size
        let stats_after = file.fstat().await?;
        assert_eq!(stats_after.size, 5, "overlay should return truncated size");

        // Verify content is truncated
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"base ", "content should be truncated");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_rename() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file (to populate overlay state)
        let _stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();

        // Rename should trigger copy-up
        overlay
            .rename(ROOT_INO, "base.txt", ROOT_INO, "renamed.txt")
            .await?;

        // Base file should still exist (we don't modify base)
        assert!(
            base_dir.path().join("base.txt").exists(),
            "base file should still exist"
        );

        // Old name should be gone in overlay (whiteout)
        assert!(
            overlay.lookup(ROOT_INO, "base.txt").await?.is_none(),
            "old name should be gone"
        );

        // New name should exist in overlay
        let renamed_stats = overlay.lookup(ROOT_INO, "renamed.txt").await?.unwrap();
        assert!(renamed_stats.is_file());

        // Content should be preserved
        let file = overlay.open(renamed_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"base content",
            "content should be preserved after rename"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_rename_base_directory_returns_exdev_and_preserves_contents() -> Result<()>
    {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        let subdir_stats = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        assert!(
            subdir_stats.is_directory(),
            "test fixture subdir should be a base-layer directory"
        );

        let err = overlay
            .rename(ROOT_INO, "subdir", ROOT_INO, "renamed_subdir")
            .await
            .expect_err("renaming a base-layer directory must return EXDEV");
        match err {
            Error::Fs(FsError::CrossDevice) => {}
            other => panic!("expected CrossDevice/EXDEV, got {other:?}"),
        }
        assert_eq!(FsError::CrossDevice.to_errno(), libc::EXDEV);

        let original_stats = overlay
            .lookup(ROOT_INO, "subdir")
            .await?
            .expect("source directory must remain visible after EXDEV");
        assert!(original_stats.is_directory());
        assert!(
            overlay.lookup(ROOT_INO, "renamed_subdir").await?.is_none(),
            "failed rename must not create the destination"
        );

        let nested_stats = overlay
            .lookup(original_stats.ino, "nested.txt")
            .await?
            .expect("base child must remain visible at original path");
        let nested_file = overlay.open(nested_stats.ino, libc::O_RDONLY).await?;
        assert_eq!(nested_file.pread(0, 100).await?, b"nested");
        assert_eq!(
            std::fs::read(base_dir.path().join("subdir/nested.txt"))?,
            b"nested",
            "base backing file must remain unchanged"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_rename_merged_base_dir_returns_exdev() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("base_dir/sub"))?;
        std::fs::write(base_dir.path().join("base_dir/base.txt"), b"base root")?;
        std::fs::write(
            base_dir.path().join("base_dir/sub/nested.txt"),
            b"base nested",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "base_dir").await?.unwrap();
        assert!(
            dir_stats.is_directory(),
            "test fixture base_dir should be a base-origin directory"
        );

        let (_new_stats, new_file) = overlay
            .create_file(dir_stats.ino, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        new_file.pwrite(0, b"delta child").await?;

        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"base.txt".to_string()),
            "merged directory should still show base child before rename"
        );
        assert!(
            entries.contains(&"new.txt".to_string()),
            "merged directory should show delta child before rename"
        );
        assert!(
            entries.contains(&"sub".to_string()),
            "merged directory should still show base subdirectory before rename"
        );

        let err = overlay
            .rename(ROOT_INO, "base_dir", ROOT_INO, "moved")
            .await
            .expect_err("renaming a merged base-origin directory must return EXDEV");
        match err {
            Error::Fs(FsError::CrossDevice) => {}
            other => panic!("expected CrossDevice/EXDEV, got {other:?}"),
        }

        let original_stats = overlay
            .lookup(ROOT_INO, "base_dir")
            .await?
            .expect("source directory must remain visible after EXDEV");
        assert!(original_stats.is_directory());
        assert!(
            overlay.lookup(ROOT_INO, "moved").await?.is_none(),
            "failed merged-dir rename must not create the destination"
        );

        let base_child = overlay
            .lookup(original_stats.ino, "base.txt")
            .await?
            .expect("base child must remain visible at original path");
        let base_file = overlay.open(base_child.ino, libc::O_RDONLY).await?;
        assert_eq!(base_file.pread(0, 100).await?, b"base root");

        let new_child = overlay
            .lookup(original_stats.ino, "new.txt")
            .await?
            .expect("delta child must remain visible at original path");
        let new_file = overlay.open(new_child.ino, libc::O_RDONLY).await?;
        assert_eq!(new_file.pread(0, 100).await?, b"delta child");

        let subdir = overlay
            .lookup(original_stats.ino, "sub")
            .await?
            .expect("base subdirectory must remain visible at original path");
        let nested = overlay
            .lookup(subdir.ino, "nested.txt")
            .await?
            .expect("nested base child must remain visible at original path");
        let nested_file = overlay.open(nested.ino, libc::O_RDONLY).await?;
        assert_eq!(nested_file.pread(0, 100).await?, b"base nested");

        assert_eq!(
            std::fs::read(base_dir.path().join("base_dir/base.txt"))?,
            b"base root",
            "base backing file must remain unchanged"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("base_dir/sub/nested.txt"))?,
            b"base nested",
            "nested base backing file must remain unchanged"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_rename_delta_origin_directory_succeeds() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let dir_stats = overlay.mkdir(ROOT_INO, "delta_dir", 0o755, 0, 0).await?;
        let (_child_stats, child_file) = overlay
            .create_file(dir_stats.ino, "child.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        child_file.pwrite(0, b"delta child").await?;

        overlay
            .rename(ROOT_INO, "delta_dir", ROOT_INO, "moved_delta")
            .await?;

        assert!(
            overlay.lookup(ROOT_INO, "delta_dir").await?.is_none(),
            "delta-origin source directory should be gone after rename"
        );

        let moved = overlay
            .lookup(ROOT_INO, "moved_delta")
            .await?
            .expect("delta-origin destination directory should exist after rename");
        assert!(moved.is_directory());

        let child = overlay
            .lookup(moved.ino, "child.txt")
            .await?
            .expect("delta-origin child should move with the directory");
        let file = overlay.open(child.ino, libc::O_RDONLY).await?;
        assert_eq!(file.pread(0, 100).await?, b"delta child");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_nested_file() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup nested file in subdir
        let subdir_stats = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        let nested_stats = overlay
            .lookup(subdir_stats.ino, "nested.txt")
            .await?
            .unwrap();

        // Open and modify (triggers copy-up, should also create parent dir in delta)
        let file = overlay.open(nested_stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified nested").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("subdir/nested.txt"))?;
        assert_eq!(
            base_content, b"nested",
            "base nested file should be unchanged"
        );

        // Verify overlay returns modified content
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"modified nested",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_symlink() -> Result<()> {
        // Create overlay with a symlink in base
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("target.txt"), b"target content")?;
        std::os::unix::fs::symlink("target.txt", base_dir.path().join("link.txt"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup symlink
        let link_stats = overlay.lookup(ROOT_INO, "link.txt").await?.unwrap();
        assert!(link_stats.is_symlink());

        // Read the symlink target
        let target = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(target, "target.txt");

        // chmod on symlink triggers copy-up
        overlay.chmod(link_stats.ino, 0o755).await?;

        // Verify symlink target is preserved after copy-up
        let target_after = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(
            target_after, "target.txt",
            "symlink target should be preserved after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_file_in_deeply_nested_base_dir() -> Result<()> {
        // This test reproduces a bug where ensure_parent_dirs uses delta inodes
        // to lookup in base layer, which breaks for paths deeper than one level.
        //
        // Setup: base has /a/b/c/ directory structure
        // Test: create a new file at /a/b/c/new.txt
        // Bug: ensure_parent_dirs would use delta inode for "a" to lookup "b" in base
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory());
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(b_stats.is_directory());
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(c_stats.is_directory());

        // Create a new file in the deeply nested directory
        // This should trigger ensure_parent_dirs to create /a/b/c in delta
        let (new_stats, file) = overlay
            .create_file(c_stats.ino, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify the file was created
        assert!(new_stats.is_file());

        // Verify we can read it back
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"new content");

        // Verify the existing file in base is still accessible
        let existing_stats = overlay.lookup(c_stats.ino, "existing.txt").await?.unwrap();
        let existing_file = overlay.open(existing_stats.ino, libc::O_RDONLY).await?;
        let existing_content = existing_file.pread(0, 100).await?;
        assert_eq!(existing_content, b"existing");

        // Verify base is unchanged
        assert!(base_dir.path().join("a/b/c/existing.txt").exists());
        assert!(!base_dir.path().join("a/b/c/new.txt").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_mkdir_in_deeply_nested_base_dir() -> Result<()> {
        // Similar test but for mkdir instead of create_file
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();

        // Create a new subdirectory in the deeply nested directory
        let new_dir_stats = overlay.mkdir(c_stats.ino, "newdir", 0o755, 0, 0).await?;
        assert!(new_dir_stats.is_directory());

        // Verify we can create a file inside the new directory
        let (file_stats, file) = overlay
            .create_file(new_dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"nested file").await?;
        assert!(file_stats.is_file());

        // Verify base is unchanged
        assert!(!base_dir.path().join("a/b/c/newdir").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_lookup_after_mkdir_in_base_parent() -> Result<()> {
        // This test reproduces a bug where lookup uses delta root (inode 1)
        // when parent is in Base layer, instead of walking the delta path.
        //
        // Scenario (mimics FUSE behavior):
        // 1. Lookup "target" in root → gets base layer inode
        // 2. mkdir("debug") inside "target" → creates /target/debug in delta
        // 3. Lookup "debug" in "target" → should find it, but bug causes it to
        //    look at delta root instead of delta's "/target"
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("target"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup "target" - this creates a Base layer mapping
        let target_stats = overlay.lookup(ROOT_INO, "target").await?.unwrap();
        assert!(target_stats.is_directory());

        // Step 2: Create "debug" inside "target"
        // This should create /target in delta, then /target/debug in delta
        let debug_stats = overlay
            .mkdir(target_stats.ino, "debug", 0o755, 0, 0)
            .await?;
        assert!(debug_stats.is_directory());

        // Step 3: Lookup "debug" inside "target" - this is where the bug manifests!
        // The bug: lookup uses delta root (1) when parent is Base layer,
        // so it looks for "debug" at delta root instead of delta's "/target"
        let debug_lookup = overlay.lookup(target_stats.ino, "debug").await?;
        assert!(
            debug_lookup.is_some(),
            "Should find 'debug' inside 'target' after mkdir"
        );
        assert!(debug_lookup.unwrap().is_directory());

        // Also verify we can create files inside the new directory
        let (file_stats, file) = overlay
            .create_file(debug_stats.ino, "test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"test content").await?;
        assert!(file_stats.is_file());

        // And lookup should find the file too
        let file_lookup = overlay.lookup(debug_stats.ino, "test.txt").await?;
        assert!(
            file_lookup.is_some(),
            "Should find 'test.txt' inside 'debug'"
        );

        Ok(())
    }

    /// Test that lookup in a base subdirectory does not return an unrelated
    /// delta entry with the same name from a wrong parent.
    ///
    /// Reproduces the ENOTDIR bug:
    ///   1. Base has /crates/agentfs-core/ (directories)
    ///   2. Delta has a *file* named "rust" under delta root (from some unrelated op)
    ///   3. lookup(sdk_ino, "rust") should return the base *directory*, not the delta file
    ///
    /// The bug: when parent is Base layer, the delta path walk breaks early
    /// (because "sdk" doesn't exist in delta) and uses delta root as parent.
    /// Then delta.lookup(root, "rust") finds the unrelated file and returns it.
    #[tokio::test]
    async fn test_overlay_lookup_base_subdir_not_shadowed_by_wrong_delta_parent() -> Result<()> {
        // Base: /crates/agentfs-core/Cargo.toml (nested directories + file)
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("crates/agentfs-core"))?;
        std::fs::write(
            base_dir.path().join("crates/agentfs-core/Cargo.toml"),
            b"[package]\nname = \"test\"",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a *file* named "agentfs-core" at the overlay root (in delta).
        // This is the entry that could shadow the base directory if the delta
        // path walk uses the wrong parent inode.
        let (_file_stats, file) = overlay
            .create_file(ROOT_INO, "agentfs-core", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"this is a file, not a directory").await?;

        // Lookup "crates" from root — should be a base directory
        let crates_stats = overlay.lookup(ROOT_INO, "crates").await?.unwrap();
        assert!(crates_stats.is_directory(), "crates should be a directory");

        // Lookup "agentfs-core" under "crates" — MUST return the base
        // *directory*, not the delta *file* with the same name under root.
        let core_stats = overlay
            .lookup(crates_stats.ino, "agentfs-core")
            .await?
            .unwrap();
        assert!(
            core_stats.is_directory(),
            "crates/agentfs-core should be a directory from base, not the file from delta root"
        );

        // Verify we can traverse further into crates/agentfs-core/Cargo.toml
        let toml_stats = overlay.lookup(core_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml should be a file");

        Ok(())
    }

    /// Test that readdir_plus and lookup agree on entry types for base dirs.
    ///
    /// readdir_plus for a Base-layer directory only returns base entries,
    /// while lookup checks delta first. They must agree on types.
    #[tokio::test]
    async fn test_overlay_readdir_plus_consistent_with_lookup_for_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("crates/agentfs-core"))?;
        std::fs::write(base_dir.path().join("crates/agentfs-core/lib.rs"), b"fn main() {}")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a file named "agentfs-core" at the root in delta (wrong-parent scenario)
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "agentfs-core", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy").await?;

        // Lookup "crates" to get its overlay inode
        let crates_stats = overlay.lookup(ROOT_INO, "crates").await?.unwrap();

        // readdir_plus on "crates" should list "agentfs-core" as a directory
        let entries = overlay
            .readdir_plus(crates_stats.ino)
            .await?
            .expect("readdir_plus should succeed on crates");
        let core_entry = entries.iter().find(|e| e.name == "agentfs-core");
        assert!(
            core_entry.is_some(),
            "readdir_plus should list 'agentfs-core'"
        );
        assert!(
            core_entry.unwrap().stats.is_directory(),
            "readdir_plus should report 'agentfs-core' as directory"
        );

        // lookup on "crates" for "agentfs-core" should also return a directory
        let core_lookup = overlay
            .lookup(crates_stats.ino, "agentfs-core")
            .await?
            .unwrap();
        assert!(
            core_lookup.is_directory(),
            "lookup should report 'agentfs-core' as directory, matching readdir_plus"
        );

        Ok(())
    }

    /// Test lookup through deeply nested base directories when an unrelated
    /// file exists at an intermediate name in the delta root.
    ///
    /// Base: /a/b/c/file.txt
    /// Delta root has file named "b"
    /// lookup(a_ino, "b") must return the base directory, not the delta file.
    #[tokio::test]
    async fn test_overlay_lookup_deep_nesting_with_delta_name_collision() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/file.txt"), b"deep content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create files named "b" and "c" at delta root — potential collisions
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "b", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy b").await?;
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "c", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy c").await?;

        // Walk the base path: root → a → b → c → file.txt
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory(), "a should be a directory");

        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(
            b_stats.is_directory(),
            "a/b should be a directory, not the delta file 'b'"
        );

        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(
            c_stats.is_directory(),
            "a/b/c should be a directory, not the delta file 'c'"
        );

        let file_stats = overlay.lookup(c_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Read the file to verify correct traversal
        let file = overlay.open(file_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"deep content");

        Ok(())
    }

    /// Test that after a copy-up creates directories in delta, lookup still
    /// returns correct types for sibling entries in the base.
    ///
    /// Scenario:
    ///   1. Base has /workspace/core/ and /workspace/tools/ (two sibling dirs)
    ///   2. Modify a file under /workspace/core/ → triggers copy-up, creates
    ///      "workspace" and "core" dirs in delta
    ///   3. Lookup /workspace/tools/ must still work (base directory)
    #[tokio::test]
    async fn test_overlay_lookup_sibling_base_dirs_after_copy_up() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("workspace/core"))?;
        std::fs::create_dir_all(base_dir.path().join("workspace/tools"))?;
        std::fs::write(
            base_dir.path().join("workspace/core/lib.rs"),
            b"fn main() {}",
        )?;
        std::fs::write(
            base_dir.path().join("workspace/tools/main.py"),
            b"print('hi')",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to workspace/core/lib.rs and modify it (triggers copy-up)
        let workspace_stats = overlay.lookup(ROOT_INO, "workspace").await?.unwrap();
        let core_stats = overlay.lookup(workspace_stats.ino, "core").await?.unwrap();
        let lib_stats = overlay.lookup(core_stats.ino, "lib.rs").await?.unwrap();
        let lib_file = overlay.open(lib_stats.ino, libc::O_RDWR).await?;
        lib_file
            .pwrite(0, b"fn main() { println!(\"hello\"); }")
            .await?;

        // Now lookup the sibling: workspace/tools must still be a directory
        let tools_stats = overlay.lookup(workspace_stats.ino, "tools").await?.unwrap();
        assert!(
            tools_stats.is_directory(),
            "workspace/tools should still be a directory after copy-up of workspace/core/lib.rs"
        );

        // And workspace/tools/main.py must be accessible
        let main_py = overlay.lookup(tools_stats.ino, "main.py").await?.unwrap();
        assert!(main_py.is_file());
        let file = overlay.open(main_py.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"print('hi')");

        Ok(())
    }

    /// Test the exact cargo scenario: path dependency at ../crates/agentfs-core/Cargo.toml
    /// accessed after some delta writes have occurred.
    #[tokio::test]
    async fn test_overlay_cargo_path_dependency_scenario() -> Result<()> {
        // Simulate the agentfs repo structure:
        // /cli/Cargo.toml
        // /crates/agentfs-core/Cargo.toml
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("cli/src"))?;
        std::fs::write(
            base_dir.path().join("cli/Cargo.toml"),
            b"[package]\nname = \"cli\"",
        )?;
        std::fs::write(base_dir.path().join("cli/src/main.rs"), b"fn main() {}")?;
        std::fs::create_dir_all(base_dir.path().join("crates/agentfs-core/src"))?;
        std::fs::write(
            base_dir.path().join("crates/agentfs-core/Cargo.toml"),
            b"[package]\nname = \"sdk\"",
        )?;
        std::fs::write(
            base_dir.path().join("crates/agentfs-core/src/lib.rs"),
            b"pub fn hello() {}",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Simulate some writes in cli/ (like cargo creating target/)
        let cli_stats = overlay.lookup(ROOT_INO, "cli").await?.unwrap();
        let _target_stats = overlay.mkdir(cli_stats.ino, "target", 0o755, 0, 0).await?;

        // Now simulate cargo resolving ../crates/agentfs-core/Cargo.toml
        // This is the path that fails with ENOTDIR in the bug report
        let crates_stats = overlay.lookup(ROOT_INO, "crates").await?.unwrap();
        assert!(crates_stats.is_directory(), "crates must be a directory");

        let core_stats = overlay
            .lookup(crates_stats.ino, "agentfs-core")
            .await?
            .unwrap();
        assert!(
            core_stats.is_directory(),
            "crates/agentfs-core must be a directory (ENOTDIR bug)"
        );

        let toml_stats = overlay.lookup(core_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml must be a file");

        // Also verify reading the file works
        let file = overlay.open(toml_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"[package]\nname = \"sdk\"");

        Ok(())
    }

    /// Test that files created in delta layer under a base directory are visible
    /// in readdir and can be deleted with unlink.
    ///
    /// This test reproduces a bug where:
    /// 1. Base has a directory (e.g., `.git/`)
    /// 2. A file is created in that directory via overlay (e.g., `.git/index.lock`)
    /// 3. `ensure_parent_dirs` creates `.git` in delta with origin mapping
    /// 4. But the overlay inode for `.git` still has `layer: Layer::Base`
    /// 5. readdir only checks delta if layer == Delta, so the new file is invisible
    /// 6. unlink only deletes from delta if parent layer == Delta, so deletion fails
    #[tokio::test]
    async fn test_overlay_readdir_and_unlink_delta_file_in_base_dir() -> Result<()> {
        // Setup: base has a .git directory with some files
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join(".git"))?;
        std::fs::write(base_dir.path().join(".git/config"), b"[core]\n")?;
        std::fs::write(base_dir.path().join(".git/HEAD"), b"ref: refs/heads/main")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup .git directory (creates Base layer mapping)
        let git_stats = overlay.lookup(ROOT_INO, ".git").await?.unwrap();
        assert!(git_stats.is_directory());

        // Step 2: Create a new file in .git (triggers ensure_parent_dirs)
        let (lock_stats, lock_file) = overlay
            .create_file(git_stats.ino, "index.lock", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        lock_file.pwrite(0, b"lock content").await?;
        assert!(lock_stats.is_file());

        // Step 3: Verify readdir shows the new file (BUG: was invisible)
        let entries = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"index.lock".to_string()),
            "readdir should show index.lock, got: {:?}",
            entries
        );
        // Also verify base files are still visible
        assert!(entries.contains(&"config".to_string()));
        assert!(entries.contains(&"HEAD".to_string()));

        // Step 4: Verify lookup also works
        let lookup_stats = overlay.lookup(git_stats.ino, "index.lock").await?.unwrap();
        assert!(lookup_stats.is_file());

        // Step 5: Delete the file
        overlay.unlink(git_stats.ino, "index.lock").await?;

        // Step 6: Verify the file is actually gone (BUG: persisted after unlink)
        let deleted = overlay.lookup(git_stats.ino, "index.lock").await?;
        assert!(
            deleted.is_none(),
            "index.lock should be deleted, but lookup still finds it"
        );

        // Also verify readdir no longer shows it
        let entries_after = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            !entries_after.contains(&"index.lock".to_string()),
            "readdir should not show index.lock after deletion"
        );

        // Base files should still be there
        assert!(entries_after.contains(&"config".to_string()));
        assert!(entries_after.contains(&"HEAD".to_string()));

        Ok(())
    }

    /// Test readdir_plus also shows delta files in base directories.
    #[tokio::test]
    async fn test_overlay_readdir_plus_delta_file_in_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("mydir"))?;
        std::fs::write(base_dir.path().join("mydir/base.txt"), b"base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup the directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "mydir").await?.unwrap();

        // Create a file in the directory
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // readdir_plus should show both base and delta files
        let entries = overlay.readdir_plus(dir_stats.ino).await?.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"base.txt"),
            "readdir_plus should show base.txt"
        );
        assert!(
            names.contains(&"delta.txt"),
            "readdir_plus should show delta.txt"
        );

        Ok(())
    }

    /// After remount, origin mappings can leave overlay inodes tagged as
    /// Layer::Base with stale base inode numbers. Verify that base files
    /// in directories with origin mappings remain accessible.
    #[tokio::test]
    async fn test_overlay_base_file_accessible_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/keep.txt"), b"keep")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and verify base files are still accessible
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let keep = overlay.lookup(dir_stats.ino, "keep.txt").await?;
        assert!(keep.is_some(), "keep.txt should be visible after remount");

        Ok(())
    }

    /// Test unlink of a BASE file after the parent directory has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario:
    /// 1. Base has /dir/base.txt and /dir/other.txt
    /// 2. Lookup /dir/ (creates Base layer mapping)
    /// 3. Create /dir/delta.txt (triggers ensure_parent_dirs, promotes /dir/ to Delta)
    /// 4. Unlink /dir/base.txt (base file in promoted parent)
    /// 5. base.txt should be gone (whiteout must be created)
    ///
    /// Bug: The base-walk loop in unlink() returns Ok(()) when a path component
    /// lookup fails in HostFS, skipping whiteout creation.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/other.txt"), b"other content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup the directory (creates Base layer mapping)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        assert!(dir_stats.is_directory());

        // Step 2: Create a file in the directory (promotes /dir/ from Base to Delta)
        let (_delta_stats, delta_file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        delta_file.pwrite(0, b"delta content").await?;

        // Step 3: Unlink the BASE file
        overlay.unlink(dir_stats.ino, "base.txt").await?;

        // Step 4: Verify the base file is gone via lookup
        let deleted = overlay.lookup(dir_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink, but lookup still finds it"
        );

        // Step 5: Verify readdir no longer shows it
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );

        // Other files should still be visible
        assert!(entries.contains(&"other.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Unlink of a base file must create a whiteout even when the parent
    /// directory has a stale origin mapping from a previous session.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_whiteout_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and unlink the base file
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "base.txt").await?;
        assert!(
            overlay.lookup(dir_stats.ino, "base.txt").await?.is_none(),
            "base.txt should be whiteout-deleted after unlink"
        );

        Ok(())
    }

    /// Test unlink of a BASE file in a deeply nested promoted parent.
    ///
    /// Scenario: base has /a/b/file.txt, promote /a/b/ by creating a delta
    /// file there, then unlink /a/b/file.txt. The base-walk must resolve
    /// both "a" and "b" in the HostFS to find the base parent.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_nested_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b"))?;
        std::fs::write(base_dir.path().join("a/b/base.txt"), b"deep base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /a/b/ (creates Base layer mappings)
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();

        // Create a delta file in /a/b/ (promotes /a/ and /a/b/ to Delta)
        let (_stats, file) = overlay
            .create_file(b_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Unlink the base file
        overlay.unlink(b_stats.ino, "base.txt").await?;

        // Verify it's gone
        let deleted = overlay.lookup(b_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink in nested promoted parent"
        );

        let entries = overlay.readdir(b_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rmdir of a BASE directory after the parent has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario: base has /parent/emptydir/, promote /parent/ by creating a
    /// delta file, then rmdir /parent/emptydir/. The whiteout must be created
    /// so the directory doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rmdir_base_dir_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::create_dir(base_dir.path().join("parent/emptydir"))?;
        std::fs::write(base_dir.path().join("parent/keep.txt"), b"keep")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup parent directory (Base layer)
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Lookup emptydir so overlay knows about it
        let emptydir_stats = overlay.lookup(parent_stats.ino, "emptydir").await?.unwrap();
        assert!(emptydir_stats.is_directory());

        // Create a delta file in /parent/ (promotes /parent/ to Delta)
        let (_stats, file) = overlay
            .create_file(parent_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // rmdir the base directory
        overlay.rmdir(parent_stats.ino, "emptydir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "emptydir").await?;
        assert!(
            deleted.is_none(),
            "emptydir should be deleted after rmdir, but lookup still finds it"
        );

        let entries = overlay.readdir(parent_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"emptydir".to_string()),
            "readdir should not show emptydir after rmdir, got: {:?}",
            entries
        );
        assert!(entries.contains(&"keep.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rename of a BASE file creates a whiteout at the source when the
    /// parent directory has been promoted from Base to Delta layer.
    ///
    /// Scenario: base has /dir/original.txt, promote /dir/ by creating a delta
    /// file, rename /dir/original.txt to /dir/renamed.txt. The source path
    /// must get a whiteout so original.txt doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rename_base_file_whiteout_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/original.txt"), b"original")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        // Lookup original.txt so overlay has the inode
        let orig_stats = overlay
            .lookup(dir_stats.ino, "original.txt")
            .await?
            .unwrap();
        assert!(orig_stats.is_file());

        // Create a delta file to promote /dir/ from Base to Delta
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Rename the base file within the same promoted directory
        overlay
            .rename(dir_stats.ino, "original.txt", dir_stats.ino, "renamed.txt")
            .await?;

        // Verify original.txt is gone (whiteout must exist)
        let deleted = overlay.lookup(dir_stats.ino, "original.txt").await?;
        assert!(
            deleted.is_none(),
            "original.txt should be gone after rename, but lookup still finds it"
        );

        // Verify renamed.txt exists
        let renamed = overlay.lookup(dir_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        // Verify readdir shows the right state
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"original.txt".to_string()),
            "readdir should not show original.txt after rename, got: {:?}",
            entries
        );
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// After remount, unlink must clean up both the delta entry and create
    /// a whiteout for the base entry — even when the parent is tagged Delta
    /// rather than Base.
    #[tokio::test]
    async fn test_overlay_unlink_removes_delta_entry_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/file.txt"), b"original base")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: copy-up file.txt to delta via write
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let file_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let file = overlay.open(file_stats.ino, libc::O_WRONLY).await?;
        file.pwrite(0, b"modified in delta").await?;

        // Session 2: remount, unlink, recreate, verify new content
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "file.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "file.txt").await?.is_none());

        let (_stats, new_file) = overlay
            .create_file(dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        new_file.pwrite(0, b"brand new content").await?;

        let read_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let read_file = overlay.open(read_stats.ino, libc::O_RDONLY).await?;
        let content = read_file.pread(0, 1024).await?;
        assert_eq!(std::str::from_utf8(&content).unwrap(), "brand new content");

        Ok(())
    }

    /// Hard-link copy-up in session 1, then unlink source in session 2.
    /// The link target must survive even though the parent has a stale
    /// origin mapping.
    #[tokio::test]
    async fn test_overlay_link_copy_up_then_unlink_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/src.txt"), b"link source")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: hard-link triggers copy_up of src.txt
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let src_stats = overlay.lookup(dir_stats.ino, "src.txt").await?.unwrap();
        overlay
            .link(src_stats.ino, dir_stats.ino, "dst.txt")
            .await?;

        // Session 2: remount, unlink source, verify link survives
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "src.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "src.txt").await?.is_none());
        assert!(overlay.lookup(dir_stats.ino, "dst.txt").await?.is_some());

        Ok(())
    }

    /// Test rename of base file across directories where both parents have
    /// been promoted. Source directory must get a whiteout for the original
    /// file, even though the base-walk must resolve through promoted parents.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_promoted_parents() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/moveme.txt"), b"moving")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup both directories
        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();

        // Promote /src/ by creating a delta file
        let (_s, f) = overlay
            .create_file(src_stats.ino, "trigger1.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Promote /dst/ by creating a delta file
        let (_s, f) = overlay
            .create_file(dst_stats.ino, "trigger2.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Lookup moveme.txt so overlay knows it
        let moveme = overlay.lookup(src_stats.ino, "moveme.txt").await?.unwrap();
        assert!(moveme.is_file());

        // Rename across promoted parents
        overlay
            .rename(src_stats.ino, "moveme.txt", dst_stats.ino, "moved.txt")
            .await?;

        // Source must be gone (whiteout at /src/moveme.txt)
        let src_lookup = overlay.lookup(src_stats.ino, "moveme.txt").await?;
        assert!(
            src_lookup.is_none(),
            "moveme.txt should be gone from /src/ after rename"
        );

        // Destination must exist
        let dst_lookup = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            dst_lookup.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        // readdir /src/ should not show moveme.txt
        let src_entries = overlay.readdir(src_stats.ino).await?.unwrap();
        assert!(
            !src_entries.contains(&"moveme.txt".to_string()),
            "readdir /src/ should not show moveme.txt, got: {:?}",
            src_entries
        );

        Ok(())
    }

    /// Test rename of a BASE file in a deeply nested directory that has not
    /// been promoted to Delta.
    ///
    /// Scenario: base has /deep/nested/file.txt, lookup the path (Base layer
    /// only, no promotion), then rename file.txt within the same directory.
    /// The delta parent must be resolved after copy_up creates it.
    #[tokio::test]
    async fn test_overlay_rename_base_file_delta_src_parent_before_copyup() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("deep/nested"))?;
        std::fs::write(base_dir.path().join("deep/nested/file.txt"), b"content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /deep/nested/ (creates Base layer mappings)
        let deep_stats = overlay.lookup(ROOT_INO, "deep").await?.unwrap();
        let nested_stats = overlay.lookup(deep_stats.ino, "nested").await?.unwrap();
        let file_stats = overlay.lookup(nested_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Rename within same directory — parents only exist in base
        overlay
            .rename(
                nested_stats.ino,
                "file.txt",
                nested_stats.ino,
                "renamed.txt",
            )
            .await?;

        let renamed = overlay.lookup(nested_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        let original = overlay.lookup(nested_stats.ino, "file.txt").await?;
        assert!(original.is_none(), "file.txt should be gone after rename");

        let entries = overlay.readdir(nested_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt, got: {:?}",
            entries
        );
        assert!(
            !entries.contains(&"file.txt".to_string()),
            "readdir should not show file.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// Test rename of a BASE file across directories when neither parent has
    /// been promoted to Delta.
    ///
    /// Scenario: base has /src/base.txt and /dst/, neither exists in delta.
    /// Rename /src/base.txt to /dst/moved.txt. Both source and destination
    /// delta parents must be correctly resolved after copy_up.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_dirs_no_found_guard() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/base.txt"), b"source content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();
        let file_stats = overlay.lookup(src_stats.ino, "base.txt").await?.unwrap();
        assert!(file_stats.is_file());
        overlay
            .rename(src_stats.ino, "base.txt", dst_stats.ino, "moved.txt")
            .await?;

        let moved = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            moved.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        let original = overlay.lookup(src_stats.ino, "base.txt").await?;
        assert!(
            original.is_none(),
            "base.txt should be gone from /src/ after rename"
        );

        let file = overlay.open(moved.unwrap().ino, libc::O_RDONLY).await?;
        let data = file.pread(0, 1024).await?;
        assert_eq!(data, b"source content");

        Ok(())
    }

    /// Test unlink of a delta-only file does not create a spurious whiteout.
    ///
    /// Scenario: base has /dir/ (empty), create delta_only.txt in delta,
    /// unlink it, then recreate with the same name. The recreated file must
    /// be visible — no whiteout should have been left behind.
    #[tokio::test]
    async fn test_overlay_unlink_delta_only_file_no_spurious_whiteout() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta only").await?;

        overlay.unlink(dir_stats.ino, "delta_only.txt").await?;

        let deleted = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            deleted.is_none(),
            "delta_only.txt should be gone after unlink"
        );

        // Recreate with the same name
        let (_stats2, file2) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file2.pwrite(0, b"recreated").await?;

        let recreated = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            recreated.is_some(),
            "recreated delta_only.txt should be visible (no spurious whiteout)"
        );

        let f = overlay.open(recreated.unwrap().ino, libc::O_RDONLY).await?;
        let data = f.pread(0, 1024).await?;
        assert_eq!(data, b"recreated");

        Ok(())
    }

    /// Test rmdir works for directories created in delta under base parent.
    #[tokio::test]
    async fn test_overlay_rmdir_delta_dir_in_base_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::write(base_dir.path().join("parent/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup base directory
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Create a subdirectory in delta
        let subdir_stats = overlay
            .mkdir(parent_stats.ino, "newsubdir", 0o755, 0, 0)
            .await?;
        assert!(subdir_stats.is_directory());

        // Verify it exists
        let lookup = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(lookup.is_some());

        // Delete it with rmdir
        overlay.rmdir(parent_stats.ino, "newsubdir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(deleted.is_none(), "newsubdir should be deleted after rmdir");

        Ok(())
    }

    #[tokio::test]
    async fn overlay_lookup_forget_prunes_maps() -> Result<()> {
        let base_dir = tempdir()?;
        for index in 0..128 {
            std::fs::write(
                base_dir.path().join(format!("file-{index:03}.txt")),
                format!("base-{index:03}"),
            )?;
        }

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let baseline = overlay.debug_map_counts();
        let mut looked_up = Vec::new();
        for index in 0..128 {
            let name = format!("file-{index:03}.txt");
            let stats = overlay.lookup(ROOT_INO, &name).await?.unwrap();
            looked_up.push(stats.ino);
        }

        let peak = overlay.debug_map_counts();
        assert!(
            peak.inode_entries >= baseline.inode_entries + looked_up.len(),
            "lookup should create overlay mappings; baseline={baseline:?}, peak={peak:?}"
        );

        for ino in looked_up {
            overlay.forget(ino, 1).await;
        }

        let final_counts = overlay.debug_map_counts();
        assert_eq!(
            final_counts, baseline,
            "FORGET should prune lookup-only overlay mappings back to baseline"
        );

        Ok(())
    }

    #[tokio::test]
    async fn overlay_load_origins_propagates_select_errors() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;
        let conn = overlay.delta().get_connection().await?;
        conn.execute("DROP TABLE fs_origin", ()).await?;

        let result = overlay.load().await;
        assert!(
            result.is_err(),
            "load_origins must propagate a failed fs_origin SELECT"
        );

        Ok(())
    }

    #[tokio::test]
    async fn overlay_whiteout_failures_roll_back() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("unlink.txt"), b"unlink")?;
        std::fs::create_dir(base_dir.path().join("empty-dir"))?;
        std::fs::write(base_dir.path().join("rename-src.txt"), b"rename")?;
        std::fs::write(base_dir.path().join("rename-dst.txt"), b"destination")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        overlay.fail_next_whiteout_for_test("unlink injected whiteout failure");
        let unlink_result = overlay.unlink(ROOT_INO, "unlink.txt").await;
        assert!(unlink_result.is_err());
        assert!(
            overlay.lookup(ROOT_INO, "unlink.txt").await?.is_some(),
            "failed unlink must leave the base path visible"
        );
        assert_eq!(
            scalar_i64(
                &overlay,
                "SELECT COUNT(*) FROM fs_whiteout WHERE path = '/unlink.txt'"
            )
            .await?,
            0,
            "failed unlink must not leave a half-applied whiteout"
        );

        overlay.fail_next_whiteout_for_test("rmdir injected whiteout failure");
        let rmdir_result = overlay.rmdir(ROOT_INO, "empty-dir").await;
        assert!(rmdir_result.is_err());
        assert!(
            overlay.lookup(ROOT_INO, "empty-dir").await?.is_some(),
            "failed rmdir must leave the base directory visible"
        );
        assert_eq!(
            scalar_i64(
                &overlay,
                "SELECT COUNT(*) FROM fs_whiteout WHERE path = '/empty-dir'",
            )
            .await?,
            0,
            "failed rmdir must not leave a half-applied whiteout"
        );

        overlay.unlink(ROOT_INO, "rename-dst.txt").await?;
        assert!(overlay.lookup(ROOT_INO, "rename-dst.txt").await?.is_none());
        assert_eq!(
            scalar_i64(
                &overlay,
                "SELECT COUNT(*) FROM fs_whiteout WHERE path = '/rename-dst.txt'",
            )
            .await?,
            1
        );

        overlay.fail_next_whiteout_for_test("rename injected whiteout failure");
        let rename_result = overlay
            .rename(ROOT_INO, "rename-src.txt", ROOT_INO, "rename-dst.txt")
            .await;
        assert!(rename_result.is_err());
        assert!(
            overlay.lookup(ROOT_INO, "rename-src.txt").await?.is_some(),
            "failed rename must leave the source path visible"
        );
        assert!(
            overlay.lookup(ROOT_INO, "rename-dst.txt").await?.is_none(),
            "failed rename must preserve the destination whiteout"
        );
        assert_eq!(
            scalar_i64(
                &overlay,
                "SELECT COUNT(*) FROM fs_whiteout WHERE path = '/rename-dst.txt'",
            )
            .await?,
            1,
            "failed rename must roll back the attempted whiteout removal"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_origin").await?,
            0,
            "failed rename must not copy up the source before the whiteout transaction succeeds"
        );

        Ok(())
    }

    async fn scalar_i64(overlay: &OverlayFS, sql: &str) -> Result<i64> {
        let conn = overlay.delta().get_connection().await?;
        let mut rows = conn.query(sql, ()).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| Error::Internal(format!("no row for scalar query: {sql}")))?;
        Ok(row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0))
    }

    async fn insert_manual_sidecars(delta: &AgentFS, ino: i64) -> Result<()> {
        let conn = delta.get_connection().await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)",
            (ino, 9_001_i64),
        )
        .await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_partial_origin \
             (delta_ino, base_ino, base_path, base_size, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            (ino, 9_001_i64, format!("/manual-{ino}"), 0_i64, 0_i64),
        )
        .await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_chunk_override (delta_ino, chunk_index) VALUES (?, ?)",
            (ino, 0_i64),
        )
        .await?;
        Ok(())
    }

    async fn assert_no_orphan_sidecars(overlay: &OverlayFS, context: &str) -> Result<()> {
        let probes = [
            (
                "fs_origin",
                "SELECT COUNT(*) FROM fs_origin s \
                 LEFT JOIN fs_inode i ON i.ino = s.delta_ino \
                 WHERE i.ino IS NULL",
            ),
            (
                "fs_partial_origin",
                "SELECT COUNT(*) FROM fs_partial_origin s \
                 LEFT JOIN fs_inode i ON i.ino = s.delta_ino \
                 WHERE i.ino IS NULL",
            ),
            (
                "fs_chunk_override",
                "SELECT COUNT(*) FROM fs_chunk_override s \
                 LEFT JOIN fs_inode i ON i.ino = s.delta_ino \
                 WHERE i.ino IS NULL",
            ),
        ];

        for (table, sql) in probes {
            assert_eq!(
                scalar_i64(overlay, sql).await?,
                0,
                "{context} should not leave orphan rows in {table}"
            );
        }
        Ok(())
    }

    fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| {
                seed.wrapping_add((index % 251) as u8)
                    .wrapping_add((index / 251) as u8)
            })
            .collect()
    }
