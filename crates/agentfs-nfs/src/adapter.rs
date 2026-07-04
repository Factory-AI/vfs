//! NFS server adapter for AgentFS.
//!
//! This module implements nfsserve's NFSFileSystem trait on top of AgentFS's
//! FileSystem trait, enabling systems to mount AgentFS via NFS without requiring
//! FUSE or other system extensions.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::server::nfs::{
    fattr3, fileid3, filename3, ftype3, nfs_fh3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime,
    set_gid3, set_mode3, set_mtime, set_size3, set_uid3, specdata3,
};
use crate::server::vfs::{auth_unix, DirEntry, NFSFileSystem, ReadDirResult};
use agentfs_core::error::Error as SdkError;
use agentfs_core::fs::FsError;
use agentfs_core::semantics::{AckDurability, Authority, Semantics};
use agentfs_core::{
    FileSystem, Stats, TimeChange, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG,
    S_IFSOCK,
};
use async_trait::async_trait;
use uuid::Uuid;

/// Root directory inode number
const ROOT_INO: fileid3 = 1;
const WRITE_HANDLE_MAGIC: &[u8; 8] = b"AFSWRIT\0";
const PLAIN_HANDLE_LEN: usize = 16;
const WRITE_HANDLE_LEN: usize = 32;

/// Convert a fileid3 to a filesystem inode number.
fn id_to_fs_ino(id: fileid3) -> i64 {
    id as i64
}

fn random_write_handle_token() -> u64 {
    let bytes = *Uuid::new_v4().as_bytes();
    u64::from_le_bytes(bytes[0..8].try_into().expect("uuid slice length is fixed"))
}

/// Convert an SDK error to an NFS status code.
///
/// Connection pool timeouts return NFS3ERR_JUKEBOX to signal the client
/// should retry the operation later. Other errors map to NFS3ERR_IO.
fn error_to_nfsstat(e: SdkError) -> nfsstat3 {
    match e {
        SdkError::Fs(ref fs_err) => match fs_err {
            FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
            FsError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            FsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
            FsError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
            FsError::IsADirectory => nfsstat3::NFS3ERR_ISDIR,
            FsError::NameTooLong => nfsstat3::NFS3ERR_NAMETOOLONG,
            FsError::RootOperation => nfsstat3::NFS3ERR_ACCES,
            _ => nfsstat3::NFS3ERR_IO,
        },
        SdkError::ConnectionPoolTimeout => nfsstat3::NFS3ERR_JUKEBOX,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

/// NFS adapter that wraps an AgentFS FileSystem.
pub(crate) struct AgentNFS {
    /// The underlying concurrency-safe filesystem.
    fs: Arc<dyn FileSystem>,
    /// Shared semantics contract for durability and coherent attrs.
    semantics: Semantics,
    /// Server-local generation number embedded in opaque file handles.
    fh_generation: u64,
}

impl AgentNFS {
    /// Create a new NFS adapter wrapping the given filesystem.
    pub fn new(fs: Arc<dyn FileSystem>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seed = (now.as_secs() << 32) ^ u64::from(now.subsec_nanos());
        AgentNFS {
            semantics: Semantics::new(fs.clone()),
            fs,
            fh_generation: seed,
        }
    }

    fn encode_plain_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut ret = Vec::with_capacity(PLAIN_HANDLE_LEN);
        ret.extend_from_slice(&self.fh_generation.to_le_bytes());
        ret.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data: ret }
    }

    fn parse_fh(&self, fh: &nfs_fh3) -> Result<(fileid3, Option<u64>), nfsstat3> {
        if fh.data.len() != PLAIN_HANDLE_LEN && fh.data.len() != WRITE_HANDLE_LEN {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }

        let generation = u64::from_le_bytes(
            fh.data[0..8]
                .try_into()
                .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?,
        );
        if generation != self.fh_generation {
            return Err(nfsstat3::NFS3ERR_STALE);
        }

        let id = u64::from_le_bytes(
            fh.data[8..16]
                .try_into()
                .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?,
        );

        if fh.data.len() == PLAIN_HANDLE_LEN {
            return Ok((id, None));
        }

        if &fh.data[16..24] != WRITE_HANDLE_MAGIC {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        let token = u64::from_le_bytes(
            fh.data[24..32]
                .try_into()
                .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?,
        );
        Ok((id, Some(token)))
    }

    /// Convert AgentFS Stats to NFS fattr3.
    fn stats_to_fattr(&self, stats: &Stats) -> fattr3 {
        let ftype = match stats.mode & S_IFMT {
            S_IFREG => ftype3::NF3REG,
            S_IFDIR => ftype3::NF3DIR,
            S_IFLNK => ftype3::NF3LNK,
            S_IFIFO => ftype3::NF3FIFO,
            S_IFCHR => ftype3::NF3CHR,
            S_IFBLK => ftype3::NF3BLK,
            S_IFSOCK => ftype3::NF3SOCK,
            _ => ftype3::NF3REG,
        };

        // Extract major/minor from rdev for device files
        let rdev = specdata3 {
            specdata1: libc::major(stats.rdev as libc::dev_t) as u32,
            specdata2: libc::minor(stats.rdev as libc::dev_t) as u32,
        };

        fattr3 {
            ftype,
            mode: stats.mode & 0o7777,
            nlink: stats.nlink,
            uid: stats.uid,
            gid: stats.gid,
            size: stats.size as u64,
            used: stats.size as u64,
            rdev,
            fsid: 0,
            fileid: stats.ino as fileid3,
            atime: nfstime3 {
                seconds: stats.atime as u32,
                nseconds: stats.atime_nsec,
            },
            mtime: nfstime3 {
                seconds: stats.mtime as u32,
                nseconds: stats.mtime_nsec,
            },
            ctime: nfstime3 {
                seconds: stats.ctime as u32,
                nseconds: stats.ctime_nsec,
            },
        }
    }
}

#[async_trait]
impl NFSFileSystem for AgentNFS {
    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        self.encode_plain_fh(id)
    }

    fn id_to_write_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut token = random_write_handle_token();
        while !self
            .semantics
            .try_grant_write_authority_with_token(id_to_fs_ino(id), token)
        {
            token = random_write_handle_token();
        }

        let mut ret = Vec::with_capacity(WRITE_HANDLE_LEN);
        ret.extend_from_slice(&self.fh_generation.to_le_bytes());
        ret.extend_from_slice(&id.to_le_bytes());
        ret.extend_from_slice(WRITE_HANDLE_MAGIC);
        ret.extend_from_slice(&token.to_le_bytes());
        nfs_fh3 { data: ret }
    }

    fn id_to_readdirplus_fh(&self, id: fileid3) -> nfs_fh3 {
        if let Some(token) = self.semantics.authority_token_for_ino(id_to_fs_ino(id)) {
            let mut ret = Vec::with_capacity(WRITE_HANDLE_LEN);
            ret.extend_from_slice(&self.fh_generation.to_le_bytes());
            ret.extend_from_slice(&id.to_le_bytes());
            ret.extend_from_slice(WRITE_HANDLE_MAGIC);
            ret.extend_from_slice(&token.to_le_bytes());
            nfs_fh3 { data: ret }
        } else {
            self.encode_plain_fh(id)
        }
    }

    fn fh_has_write_authority(&self, fh: &nfs_fh3, id: fileid3) -> bool {
        let Ok((handle_id, Some(token))) = self.parse_fh(fh) else {
            return false;
        };
        if handle_id != id {
            return false;
        }
        self.semantics.has_write_authority(id_to_fs_ino(id), token)
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        self.parse_fh(fh).map(|(id, _)| id)
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Handle .
        if name == "." {
            return Ok(dirid);
        }

        let fs = self.fs.clone();

        // Handle .. via filesystem lookup
        if name == ".." {
            let stats = fs
                .lookup(id_to_fs_ino(dirid), "..")
                .await
                .map_err(error_to_nfsstat)?
                .ok_or(nfsstat3::NFS3ERR_NOENT)?;
            return Ok(stats.ino as fileid3);
        }

        // Verify parent is a directory
        let dir_stats = fs
            .getattr(id_to_fs_ino(dirid))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if !dir_stats.is_directory() {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        // Lookup the entry
        let stats = fs
            .lookup(id_to_fs_ino(dirid), name)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(stats.ino as fileid3)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let stats = self
            .semantics
            .stat_coherent(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let fs_ino = id_to_fs_ino(id);
        let fs = self.fs.clone();

        // Handle chmod (mode change)
        if let set_mode3::mode(mode) = setattr.mode {
            fs.chmod(fs_ino, mode).await.map_err(error_to_nfsstat)?;
        }

        // Handle chown (uid/gid change)
        let new_uid = if let set_uid3::uid(uid) = setattr.uid {
            Some(uid)
        } else {
            None
        };
        let new_gid = if let set_gid3::gid(gid) = setattr.gid {
            Some(gid)
        } else {
            None
        };
        if new_uid.is_some() || new_gid.is_some() {
            fs.chown(fs_ino, new_uid, new_gid)
                .await
                .map_err(error_to_nfsstat)?;
        }

        // Handle size change (truncate)
        if let set_size3::size(size) = setattr.size {
            let handle = self
                .semantics
                .open_cached(fs_ino, Authority::Write)
                .await
                .map_err(error_to_nfsstat)?;
            handle
                .file()
                .truncate(size)
                .await
                .map_err(error_to_nfsstat)?;
        }

        // Handle atime/mtime changes (utimensat)
        let new_atime = match setattr.atime {
            set_atime::SET_TO_CLIENT_TIME(t) => TimeChange::Set(t.seconds as i64, t.nseconds),
            set_atime::SET_TO_SERVER_TIME => TimeChange::Now,
            set_atime::DONT_CHANGE => TimeChange::Omit,
        };
        let new_mtime = match setattr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(t) => TimeChange::Set(t.seconds as i64, t.nseconds),
            set_mtime::SET_TO_SERVER_TIME => TimeChange::Now,
            set_mtime::DONT_CHANGE => TimeChange::Omit,
        };
        if !matches!(new_atime, TimeChange::Omit) || !matches!(new_mtime, TimeChange::Omit) {
            fs.utimens(fs_ino, new_atime, new_mtime)
                .await
                .map_err(error_to_nfsstat)?;
        }

        // Get updated stats
        let stats = self
            .semantics
            .stat_coherent(fs_ino)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let handle = self
            .semantics
            .open_cached(id_to_fs_ino(id), Authority::Read)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        let data = handle
            .file()
            .pread(offset, count as u64)
            .await
            .map_err(error_to_nfsstat)?;

        // Check if we've reached EOF
        let stats = handle.file().fstat().await.map_err(error_to_nfsstat)?;

        let eof = offset + data.len() as u64 >= stats.size as u64;
        Ok((data, eof))
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
        durability: AckDurability,
    ) -> Result<(fattr3, AckDurability), nfsstat3> {
        let handle = self
            .semantics
            .open_cached(id_to_fs_ino(id), Authority::Write)
            .await
            .map_err(error_to_nfsstat)?;
        let receipt = self
            .semantics
            .write_handle(&handle, offset, data, durability)
            .await
            .map_err(error_to_nfsstat)?;

        let stats = self
            .semantics
            .stat_coherent(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok((self.stats_to_fattr(&stats), receipt.durability))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Use mode from sattr3 if provided, otherwise default to 0o644
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        let fs = self.fs.clone();
        let (stats, _file) = fs
            .create_file(dir_fs_ino, name, S_IFREG | mode, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
        auth: &auth_unix,
    ) -> Result<fileid3, nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.clone();

        // Check if file already exists
        if fs
            .lookup(dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?
            .is_some()
        {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }

        // Create file with caller's uid/gid
        let (stats, _file) = fs
            .create_file(dir_fs_ino, name, S_IFREG | 0o644, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        Ok(stats.ino as fileid3)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Use mode from sattr3 if provided, otherwise default to 0o755
        let mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o755,
        };

        let fs = self.fs.clone();

        let stats = fs
            .mkdir(dir_fs_ino, name, mode, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn mknod(
        &self,
        dirid: fileid3,
        filename: &filename3,
        ftype: ftype3,
        attr: sattr3,
        rdev: specdata3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        // Use mode from sattr3 if provided, otherwise default to 0o644
        let perm_mode = match attr.mode {
            set_mode3::mode(m) => m & 0o7777,
            set_mode3::Void => 0o644,
        };

        // Convert NFS file type to SDK mode constant
        let type_mode = match ftype {
            ftype3::NF3CHR => S_IFCHR,
            ftype3::NF3BLK => S_IFBLK,
            ftype3::NF3SOCK => S_IFSOCK,
            ftype3::NF3FIFO => S_IFIFO,
            _ => return Err(nfsstat3::NFS3ERR_BADTYPE),
        };

        // Convert rdev from specdata3 (major/minor) to u64
        let rdev_val = libc::makedev(rdev.specdata1 as _, rdev.specdata2 as _) as u64;

        let fs = self.fs.clone();

        let stats = fs
            .mknod(
                dir_fs_ino,
                name,
                type_mode | perm_mode,
                rdev_val,
                auth.uid,
                auth.gid,
            )
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.clone();

        // Check if it's a file or directory and use appropriate method
        let stats = fs
            .lookup(dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if stats.is_directory() {
            fs.rmdir(dir_fs_ino, name).await.map_err(error_to_nfsstat)?;
        } else {
            fs.unlink(dir_fs_ino, name)
                .await
                .map_err(error_to_nfsstat)?;
        }
        self.semantics.invalidate_handles(stats.ino);

        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_dir_fs_ino = id_to_fs_ino(from_dirid);
        let to_dir_fs_ino = id_to_fs_ino(to_dirid);
        let from_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let to_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.clone();

        let replaced_ino = fs
            .rename_with_replaced_ino(from_dir_fs_ino, from_name, to_dir_fs_ino, to_name)
            .await
            .map_err(error_to_nfsstat)?;
        if let Some(ino) = replaced_ino {
            self.semantics.invalidate_handles(ino);
        }

        Ok(())
    }

    async fn link(
        &self,
        id: fileid3,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fattr3, nfsstat3> {
        let fs_ino = id_to_fs_ino(id);
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.clone();
        let stats = fs
            .link(fs_ino, dir_fs_ino, name)
            .await
            .map_err(error_to_nfsstat)?;

        Ok(self.stats_to_fattr(&stats))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);

        let fs = self.fs.clone();

        let page = fs
            .readdir_plus_after(dir_fs_ino, start_after as i64, max_entries.max(1))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        drop(fs);

        let mut result = ReadDirResult {
            entries: Vec::new(),
            end: page.end,
        };

        for entry in page.entries {
            let ino = entry.stats.ino as fileid3;

            result.entries.push(DirEntry {
                fileid: ino,
                name: entry.name.as_bytes().into(),
                attr: self.stats_to_fattr(&entry.stats),
            });
        }

        Ok(result)
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let dir_fs_ino = id_to_fs_ino(dirid);
        let name = std::str::from_utf8(linkname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let target = std::str::from_utf8(symlink).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;

        let fs = self.fs.clone();

        let stats = fs
            .symlink(dir_fs_ino, name, target, auth.uid, auth.gid)
            .await
            .map_err(error_to_nfsstat)?;

        let ino = stats.ino as fileid3;
        let fattr = self.stats_to_fattr(&stats);
        Ok((ino, fattr))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let fs = self.fs.clone();

        let target = fs
            .readlink(id_to_fs_ino(id))
            .await
            .map_err(error_to_nfsstat)?
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        Ok(target.into_bytes().into())
    }

    async fn finalize(&self) -> anyhow::Result<()> {
        self.semantics
            .commit_barrier(None)
            .await
            .map_err(anyhow::Error::from)?;
        self.semantics.finalize().await.map_err(anyhow::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::vfs::NFSFileSystem;
    use agentfs_core::fs::AgentFS as CoreAgentFS;
    use agentfs_core::{AgentFS, AgentFSOptions, HostFS, OverlayFS};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn test_nfs() -> AgentNFS {
        let agent = AgentFS::open(AgentFSOptions::ephemeral())
            .await
            .expect("ephemeral AgentFS opens");
        let fs: Arc<dyn FileSystem> = Arc::new(agent.fs);
        AgentNFS::new(fs)
    }

    async fn overlay_nfs_with_base_file() -> (AgentNFS, TempDir, TempDir) {
        let base_dir = tempfile::tempdir().expect("base tempdir is created");
        std::fs::write(base_dir.path().join("base.txt"), b"base").expect("base file is written");
        let base: Arc<dyn FileSystem> =
            Arc::new(HostFS::new(base_dir.path()).expect("host fs opens base dir"));

        let delta_dir = tempfile::tempdir().expect("delta tempdir is created");
        let db_path = delta_dir.path().join("delta.db");
        let delta = CoreAgentFS::new(db_path.to_str().expect("delta DB path is UTF-8"))
            .await
            .expect("delta AgentFS opens");
        let overlay = OverlayFS::new(base, delta);
        overlay
            .init(base_dir.path().to_str().expect("base path is UTF-8"))
            .await
            .expect("overlay schema initializes");
        let fs: Arc<dyn FileSystem> = Arc::new(overlay);

        (AgentNFS::new(fs), base_dir, delta_dir)
    }

    #[tokio::test]
    async fn write_handle_grants_exact_authority_but_plain_lookup_handle_does_not() {
        let nfs = test_nfs().await;

        let write_fh = nfs.id_to_write_fh(42);
        assert_eq!(write_fh.data.len(), WRITE_HANDLE_LEN);
        assert!(matches!(nfs.fh_to_id(&write_fh), Ok(42)));
        assert!(nfs.fh_has_write_authority(&write_fh, 42));
        assert!(!nfs.fh_has_write_authority(&write_fh, 43));

        let plain_fh = nfs.id_to_fh(42);
        assert_eq!(plain_fh.data.len(), PLAIN_HANDLE_LEN);
        assert!(matches!(nfs.fh_to_id(&plain_fh), Ok(42)));
        assert!(!nfs.fh_has_write_authority(&plain_fh, 42));
    }

    #[tokio::test]
    async fn write_handle_rejects_stale_bad_and_forged_tokens() {
        let nfs = test_nfs().await;
        let write_fh = nfs.id_to_write_fh(7);

        let mut stale_fh = write_fh.clone();
        stale_fh.data[0] ^= 0x80;
        assert!(matches!(
            nfs.fh_to_id(&stale_fh),
            Err(nfsstat3::NFS3ERR_STALE)
        ));
        assert!(!nfs.fh_has_write_authority(&stale_fh, 7));

        let mut bad_magic_fh = write_fh.clone();
        bad_magic_fh.data[16] ^= 0xff;
        assert!(matches!(
            nfs.fh_to_id(&bad_magic_fh),
            Err(nfsstat3::NFS3ERR_BADHANDLE)
        ));
        assert!(!nfs.fh_has_write_authority(&bad_magic_fh, 7));

        let mut forged_token_fh = write_fh.clone();
        let token = u64::from_le_bytes(
            forged_token_fh.data[24..32]
                .try_into()
                .expect("write handle token length"),
        );
        forged_token_fh.data[24..32].copy_from_slice(&token.wrapping_add(1).to_le_bytes());
        assert!(matches!(nfs.fh_to_id(&forged_token_fh), Ok(7)));
        assert!(!nfs.fh_has_write_authority(&forged_token_fh, 7));
    }

    #[tokio::test]
    async fn readdirplus_handle_reuses_live_create_write_authority() {
        let nfs = test_nfs().await;
        let write_fh = nfs.id_to_write_fh(7);
        assert!(nfs.fh_has_write_authority(&write_fh, 7));

        let readdirplus_fh = nfs.id_to_readdirplus_fh(7);
        assert!(nfs.fh_has_write_authority(&readdirplus_fh, 7));
    }

    #[tokio::test]
    async fn overlay_base_read_write_read_returns_post_write_bytes() {
        let (nfs, _base_dir, _delta_dir) = overlay_nfs_with_base_file().await;

        let ino = nfs
            .lookup(1, &b"base.txt".to_vec().into())
            .await
            .expect("base file lookup succeeds");
        let (before, before_eof) = nfs
            .read(ino, 0, 64)
            .await
            .expect("initial base read succeeds");
        assert_eq!(before, b"base");
        assert!(before_eof);

        let post_write = b"post-write";
        let (write_attrs, durability) = nfs
            .write(ino, 0, post_write, AckDurability::Committed)
            .await
            .expect("write copy-up succeeds");
        assert_eq!(durability, AckDurability::Committed);
        assert_eq!(write_attrs.size, post_write.len() as u64);

        let (after, after_eof) = nfs
            .read(ino, 0, 64)
            .await
            .expect("post-write read succeeds");
        assert_eq!(
            after, post_write,
            "read handle cached before copy-up must not serve stale base bytes"
        );
        assert!(after_eof);

        let attrs_after = nfs.getattr(ino).await.expect("post-write getattr succeeds");
        assert_eq!(attrs_after.size, post_write.len() as u64);
        assert_eq!(attrs_after.size, after.len() as u64);
    }

    #[tokio::test]
    async fn remove_and_rename_over_invalidate_write_authority() {
        let nfs = test_nfs().await;
        let auth = auth_unix {
            stamp: 0,
            machinename: b"test".to_vec(),
            uid: 1000,
            gid: 1000,
            gids: Vec::new(),
        };

        let (victim, _) = nfs
            .create(1, &b"victim.txt".to_vec().into(), sattr3::default(), &auth)
            .await
            .expect("victim create succeeds");
        let victim_fh = nfs.id_to_write_fh(victim);
        assert!(nfs.fh_has_write_authority(&victim_fh, victim));

        nfs.remove(1, &b"victim.txt".to_vec().into())
            .await
            .expect("remove succeeds");
        assert!(!nfs.fh_has_write_authority(&victim_fh, victim));

        let (replaced, _) = nfs
            .create(
                1,
                &b"replaced.txt".to_vec().into(),
                sattr3::default(),
                &auth,
            )
            .await
            .expect("replaced create succeeds");
        let replaced_fh = nfs.id_to_write_fh(replaced);
        let (incoming, _) = nfs
            .create(
                1,
                &b"incoming.txt".to_vec().into(),
                sattr3::default(),
                &auth,
            )
            .await
            .expect("incoming create succeeds");
        let incoming_fh = nfs.id_to_write_fh(incoming);

        nfs.rename(
            1,
            &b"incoming.txt".to_vec().into(),
            1,
            &b"replaced.txt".to_vec().into(),
        )
        .await
        .expect("rename-over succeeds");

        assert!(!nfs.fh_has_write_authority(&replaced_fh, replaced));
        assert!(nfs.fh_has_write_authority(&incoming_fh, incoming));
    }
}
