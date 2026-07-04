use super::nfs;
use super::nfs::*;
pub(crate) use super::rpc::auth_unix;
use agentfs_core::semantics::AckDurability;
use async_trait::async_trait;
use std::cmp::Ordering;
use std::sync::OnceLock;
use std::time::SystemTime;
#[derive(Default, Debug)]
pub struct DirEntrySimple {
    pub fileid: fileid3,
    pub cookie: cookie3,
    pub name: filename3,
}
#[derive(Default, Debug)]
pub struct ReadDirSimpleResult {
    pub entries: Vec<DirEntrySimple>,
    pub end: bool,
}

#[derive(Default, Debug)]
pub struct DirEntry {
    pub fileid: fileid3,
    pub cookie: cookie3,
    pub name: filename3,
    pub attr: fattr3,
}
#[derive(Default, Debug)]
pub struct ReadDirResult {
    pub entries: Vec<DirEntry>,
    pub end: bool,
}

impl ReadDirSimpleResult {
    fn from_readdir_result(result: &ReadDirResult) -> ReadDirSimpleResult {
        let entries: Vec<DirEntrySimple> = result
            .entries
            .iter()
            .map(|e| DirEntrySimple {
                fileid: e.fileid,
                cookie: e.cookie,
                name: e.name.clone(),
            })
            .collect();
        ReadDirSimpleResult {
            entries,
            end: result.end,
        }
    }
}

static GENERATION_NUMBER: OnceLock<u64> = OnceLock::new();

fn get_generation_number() -> u64 {
    *GENERATION_NUMBER.get_or_init(|| {
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    })
}

/// The basic API to implement to provide an NFS file system
///
/// Opaque FH
/// ---------
/// Files are only uniquely identified by a 64-bit file id. (basically an inode number)
/// We automatically produce internally the opaque filehandle which is comprised of
///  - A 64-bit generation number derived from the server startup time
///   (i.e. so the opaque file handle expires when the NFS server restarts)
///  - The 64-bit file id
//
/// readdir pagination
/// ------------------
/// We do not use cookie verifier. We just use the start_after.  The
/// implementation should allow startat to start at any position. That is,
/// the next query to readdir may be the last entry in the previous readdir
/// response.
//
/// There is a wierd annoying thing about readdir that limits the number
/// of bytes in the response (instead of the number of entries). The caller
/// will have to truncate the readdir response / issue more calls to readdir
/// accordingly to fill up the expected number of bytes without exceeding it.
//
/// Other requirements
/// ------------------
///  getattr needs to be fast. NFS uses that a lot
//
///  The 0 fileid is reserved and should not be used
///
#[async_trait]
pub trait NFSFileSystem: Sync {
    /// Returns the ID the of the root directory "/"
    fn root_dir(&self) -> fileid3;
    /// Look up the id of a path in a directory
    ///
    /// i.e. given a directory dir/ containing a file a.txt
    /// this may call lookup(id_of("dir/"), "a.txt")
    /// and this should return the id of the file "dir/a.txt"
    ///
    /// This method should be fast as it is used very frequently.
    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3>;

    /// Returns the attributes of an id.
    /// This method should be fast as it is used very frequently.
    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3>;

    /// Sets the attributes of an id.
    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3>;

    /// Reads the contents of a file returning (bytes, EOF)
    /// Note that offset/count may go past the end of the file and that
    /// in that case, all bytes till the end of file are returned.
    /// EOF must be flagged if the end of the file is reached by the read.
    async fn read(&self, id: fileid3, offset: u64, count: u32)
        -> Result<(Vec<u8>, bool), nfsstat3>;

    /// Writes the contents of a file.
    /// Note that offset/count may go past the end of the file and that
    /// in that case, the file is extended.
    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
        durability: AckDurability,
    ) -> Result<(fattr3, AckDurability), nfsstat3>;

    /// Creates a file with the following attributes.
    /// The auth parameter contains the caller's credentials for ownership.
    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3>;

    /// Creates a file if it does not already exist
    /// The auth parameter contains the caller's credentials for ownership.
    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
        auth: &auth_unix,
    ) -> Result<fileid3, nfsstat3>;

    /// Makes a directory with the following attributes.
    /// The auth parameter contains the caller's credentials for ownership.
    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
        attr: sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3>;

    /// Creates a special file (device, fifo, socket) with the given attributes.
    /// The auth parameter contains the caller's credentials for ownership.
    async fn mknod(
        &self,
        dirid: fileid3,
        filename: &filename3,
        ftype: ftype3,
        attr: sattr3,
        rdev: specdata3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3>;

    /// Removes a file.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3>;

    /// Removes a file.
    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3>;

    /// Creates a hard link to an existing file.
    async fn link(
        &self,
        id: fileid3,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fattr3, nfsstat3>;

    /// Returns the contents of a directory with pagination.
    /// Directory listing should be deterministic.
    /// Up to max_entries may be returned, and start_after is the opaque cookie
    /// returned on the prior page's last entry.
    ///
    /// For instance if the directory returns cookies [1,6,2,11,8,9]
    /// and start_after=6, readdir should return 2,11,8,...
    //
    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3>;

    /// Simple version of readdir.
    /// Only need to return filename and id
    async fn readdir_simple(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        count: usize,
    ) -> Result<ReadDirSimpleResult, nfsstat3> {
        Ok(ReadDirSimpleResult::from_readdir_result(
            &self.readdir(dirid, start_after, count).await?,
        ))
    }

    /// Makes a symlink with the following attributes.
    /// The auth parameter contains the caller's credentials for ownership.
    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        attr: &sattr3,
        auth: &auth_unix,
    ) -> Result<(fileid3, fattr3), nfsstat3>;

    /// Reads a symlink
    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3>;

    /// Get static file system Information
    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let dir_attr: nfs::post_op_attr = match self.getattr(root_fileid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };

        let res = fsinfo3 {
            obj_attributes: dir_attr,
            rtmax: 1024 * 1024,
            rtpref: 1024 * 1024,
            rtmult: 1024 * 1024,
            wtmax: 1024 * 1024,
            wtpref: 1024 * 1024,
            wtmult: 1024 * 1024,
            dtpref: 1024 * 1024,
            maxfilesize: 128 * 1024 * 1024 * 1024,
            time_delta: nfs::nfstime3 {
                seconds: 0,
                nseconds: 1000000,
            },
            properties: nfs::FSF_SYMLINK | nfs::FSF_HOMOGENEOUS | nfs::FSF_CANSETTIME,
        };
        Ok(res)
    }

    /// Finalize a clean server shutdown after the accept loop has stopped and
    /// all connection tasks have exited.
    async fn finalize(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Converts the fileid to an opaque NFS file handle. Optional.
    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let gennum = get_generation_number();
        let mut ret: Vec<u8> = Vec::new();
        ret.extend_from_slice(&gennum.to_le_bytes());
        ret.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data: ret }
    }

    /// Converts the fileid to an opaque NFS file handle that carries write
    /// authority captured by a successful CREATE response.
    ///
    /// NFSv3 has no OPEN/CLOSE RPC, so clients commonly continue writing
    /// through the file handle returned by CREATE. Implementations that can
    /// encode per-handle authority should override this method and
    /// `fh_has_write_authority`; the default preserves stateless NFS behavior.
    fn id_to_write_fh(&self, id: fileid3) -> nfs_fh3 {
        self.id_to_fh(id)
    }

    /// Converts the fileid to a handle suitable for READDIRPLUS.
    ///
    /// Implementations with live CREATE write-authority tokens can override
    /// this to keep a client-side node refresh from downgrading the handle to
    /// a plain mode-bit-gated handle.
    fn id_to_readdirplus_fh(&self, id: fileid3) -> nfs_fh3 {
        self.id_to_fh(id)
    }

    /// Returns whether this exact opaque file handle carries write authority
    /// captured at CREATE time for the resolved fileid.
    fn fh_has_write_authority(&self, _fh: &nfs_fh3, _id: fileid3) -> bool {
        false
    }

    /// Converts an opaque NFS file handle to a fileid.  Optional.
    fn fh_to_id(&self, id: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        if id.data.len() != 16 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        let gen = u64::from_le_bytes(id.data[0..8].try_into().unwrap());
        let id = u64::from_le_bytes(id.data[8..16].try_into().unwrap());
        let gennum = get_generation_number();
        match gen.cmp(&gennum) {
            Ordering::Less => Err(nfsstat3::NFS3ERR_STALE),
            Ordering::Greater => Err(nfsstat3::NFS3ERR_BADHANDLE),
            Ordering::Equal => Ok(id),
        }
    }
    /// Converts a complete path to a fileid.  Optional.
    /// The default implementation walks the directory structure with lookup()
    async fn path_to_id(&self, path: &[u8]) -> Result<fileid3, nfsstat3> {
        let splits = path.split(|&r| r == b'/');
        let mut fid = self.root_dir();
        for component in splits {
            if component.is_empty() {
                continue;
            }
            fid = self.lookup(fid, &component.into()).await?;
        }
        Ok(fid)
    }

    fn serverid(&self) -> cookieverf3 {
        let gennum = get_generation_number();
        gennum.to_le_bytes()
    }
}
