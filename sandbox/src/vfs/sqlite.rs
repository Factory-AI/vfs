use super::file::{BoxedFileOps, FileOps};
use super::{Vfs, VfsError, VfsResult};
use agentfs_sdk::{filesystem::AgentFS, FileSystem};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Root inode number
const ROOT_INO: i64 = 1;

/// A SQLite-backed virtual filesystem using the AgentFS SDK
///
/// This implements a full POSIX-like filesystem stored in a SQLite database,
/// using the agentfs-sdk Filesystem module.
#[derive(Clone)]
pub struct SqliteVfs {
    /// The filesystem from the SDK
    fs: Arc<dyn FileSystem>,
    /// The virtual path as seen by the sandboxed process
    mount_point: PathBuf,
}

impl SqliteVfs {
    /// Create a new SQLite VFS
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file
    /// * `mount_point` - The virtual path seen by the guest (e.g., "/agent")
    pub async fn new(db_path: impl AsRef<Path>, mount_point: PathBuf) -> VfsResult<Self> {
        let db_path_str = db_path
            .as_ref()
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid database path".to_string()))?;

        let fs = AgentFS::new(db_path_str)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to create filesystem: {}", e)))?;

        Ok(Self {
            fs: Arc::new(fs) as Arc<dyn FileSystem>,
            mount_point,
        })
    }

    /// Get the mount point path
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Translate a sandbox path to a relative path for the SDK
    fn translate_to_relative(&self, path: &Path) -> VfsResult<String> {
        let path_str = path
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid path".to_string()))?;

        let mount_str = self
            .mount_point
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid mount point".to_string()))?;

        // Remove mount point prefix to get relative path
        let relative = if path_str == mount_str {
            "/"
        } else if let Some(rel) = path_str.strip_prefix(&format!("{}/", mount_str)) {
            &format!("/{}", rel)
        } else {
            return Err(VfsError::NotFound);
        };

        Ok(relative.to_string())
    }

    /// Resolve a path to an inode by walking from root
    async fn resolve_path(&self, path: &str) -> VfsResult<i64> {
        if path == "/" {
            return Ok(ROOT_INO);
        }

        let mut current_ino = ROOT_INO;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            let stats = self
                .fs
                .lookup(current_ino, component)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to lookup: {}", e)))?
                .ok_or(VfsError::NotFound)?;
            current_ino = stats.ino;
        }

        Ok(current_ino)
    }

    /// Resolve a path to (parent_ino, name)
    fn split_path(path: &str) -> VfsResult<(String, String)> {
        if path == "/" {
            return Err(VfsError::InvalidInput("Cannot split root path".to_string()));
        }

        let path = path.trim_end_matches('/');
        if let Some(pos) = path.rfind('/') {
            let parent = if pos == 0 { "/" } else { &path[..pos] };
            let name = &path[pos + 1..];
            Ok((parent.to_string(), name.to_string()))
        } else {
            Ok(("/".to_string(), path.to_string()))
        }
    }
}

#[async_trait::async_trait]
impl Vfs for SqliteVfs {
    fn translate_path(&self, path: &Path) -> VfsResult<PathBuf> {
        // For virtual VFS, we just validate the path is under our mount point
        let path_str = path
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid path".to_string()))?;

        let mount_str = self
            .mount_point
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid mount point".to_string()))?;

        if path_str.starts_with(mount_str) {
            Ok(path.to_path_buf())
        } else {
            Err(VfsError::NotFound)
        }
    }

    fn is_virtual(&self) -> bool {
        true
    }

    async fn open(&self, path: &Path, flags: i32, _mode: u32) -> VfsResult<BoxedFileOps> {
        let relative_path = self.translate_to_relative(path)?;

        // Try to resolve the path to get stats
        let stats_result = if relative_path == "/" {
            self.fs.getattr(ROOT_INO).await
        } else {
            let (parent_path, name) = Self::split_path(&relative_path)?;
            let parent_ino = self.resolve_path(&parent_path).await?;
            self.fs.lookup(parent_ino, &name).await
        };

        let stats = stats_result.map_err(|e| VfsError::Other(format!("Failed to stat: {}", e)))?;

        match stats {
            Some(stats) => {
                if stats.is_directory() {
                    Ok(Arc::new(SqliteDirectoryOps {
                        fs: self.fs.clone(),
                        ino: stats.ino,
                        path: relative_path,
                        flags: Mutex::new(flags),
                        entries: Arc::new(Mutex::new(None)),
                        position: Arc::new(Mutex::new(0)),
                    }))
                } else {
                    // If O_TRUNC is set, skip reading the file and use empty data
                    let data = if flags & libc::O_TRUNC != 0 {
                        Vec::new()
                    } else {
                        // Read file content using open + pread
                        let file =
                            self.fs.open(stats.ino, libc::O_RDONLY).await.map_err(|e| {
                                VfsError::Other(format!("Failed to open file: {}", e))
                            })?;
                        file.pread(0, stats.size as u64)
                            .await
                            .map_err(|e| VfsError::Other(format!("Failed to read file: {}", e)))?
                    };
                    Ok(Arc::new(SqliteFileOps {
                        fs: self.fs.clone(),
                        ino: stats.ino,
                        path: relative_path,
                        data: Arc::new(Mutex::new(data)),
                        offset: Arc::new(Mutex::new(0)),
                        flags: Mutex::new(flags),
                        dirty: Arc::new(Mutex::new(flags & libc::O_TRUNC != 0)),
                    }))
                }
            }
            None => {
                // File doesn't exist - check if O_CREAT is set
                if flags & libc::O_CREAT != 0 {
                    let data = Vec::new();

                    // We don't have an inode yet - use 0 as placeholder
                    // The actual file will be created on fsync/close
                    Ok(Arc::new(SqliteFileOps {
                        fs: self.fs.clone(),
                        ino: 0, // Will be assigned when created
                        path: relative_path,
                        data: Arc::new(Mutex::new(data)),
                        offset: Arc::new(Mutex::new(0)),
                        flags: Mutex::new(flags),
                        dirty: Arc::new(Mutex::new(true)), // Mark as dirty so it gets written on close
                    }))
                } else {
                    // File doesn't exist and O_CREAT not set
                    Err(VfsError::NotFound)
                }
            }
        }
    }

    async fn stat(&self, path: &Path) -> VfsResult<libc::stat> {
        let relative_path = self.translate_to_relative(path)?;

        let ino = self.resolve_path(&relative_path).await?;
        let stats = self
            .fs
            .getattr(ino)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to getattr: {}", e)))?
            .ok_or(VfsError::NotFound)?;

        // Use MaybeUninit to construct libc::stat safely
        let mut stat: std::mem::MaybeUninit<libc::stat> = std::mem::MaybeUninit::zeroed();
        unsafe {
            let stat_ptr = stat.as_mut_ptr();
            (*stat_ptr).st_dev = 0;
            (*stat_ptr).st_ino = stats.ino as u64;
            (*stat_ptr).st_nlink = stats.nlink.into();
            (*stat_ptr).st_mode = stats.mode;
            (*stat_ptr).st_uid = stats.uid;
            (*stat_ptr).st_gid = stats.gid;
            (*stat_ptr).st_rdev = 0;
            (*stat_ptr).st_size = stats.size;
            (*stat_ptr).st_blksize = 4096;
            (*stat_ptr).st_blocks = (stats.size + 4095) / 4096;
            (*stat_ptr).st_atime = stats.atime;
            (*stat_ptr).st_atime_nsec = 0;
            (*stat_ptr).st_mtime = stats.mtime;
            (*stat_ptr).st_mtime_nsec = 0;
            (*stat_ptr).st_ctime = stats.ctime;
            (*stat_ptr).st_ctime_nsec = 0;
            Ok(stat.assume_init())
        }
    }

    async fn lstat(&self, path: &Path) -> VfsResult<libc::stat> {
        let relative_path = self.translate_to_relative(path)?;

        // For lstat, we use lookup which doesn't follow symlinks
        let stats = if relative_path == "/" {
            self.fs
                .getattr(ROOT_INO)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to getattr: {}", e)))?
                .ok_or(VfsError::NotFound)?
        } else {
            let (parent_path, name) = Self::split_path(&relative_path)?;
            let parent_ino = self.resolve_path(&parent_path).await?;
            self.fs
                .lookup(parent_ino, &name)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to lookup: {}", e)))?
                .ok_or(VfsError::NotFound)?
        };

        // Use MaybeUninit to construct libc::stat safely
        let mut stat: std::mem::MaybeUninit<libc::stat> = std::mem::MaybeUninit::zeroed();
        unsafe {
            let stat_ptr = stat.as_mut_ptr();
            (*stat_ptr).st_dev = 0;
            (*stat_ptr).st_ino = stats.ino as u64;
            (*stat_ptr).st_nlink = stats.nlink.into();
            (*stat_ptr).st_mode = stats.mode;
            (*stat_ptr).st_uid = stats.uid;
            (*stat_ptr).st_gid = stats.gid;
            (*stat_ptr).st_rdev = 0;
            (*stat_ptr).st_size = stats.size;
            (*stat_ptr).st_blksize = 4096;
            (*stat_ptr).st_blocks = (stats.size + 4095) / 4096;
            (*stat_ptr).st_atime = stats.atime;
            (*stat_ptr).st_atime_nsec = 0;
            (*stat_ptr).st_mtime = stats.mtime;
            (*stat_ptr).st_mtime_nsec = 0;
            (*stat_ptr).st_ctime = stats.ctime;
            (*stat_ptr).st_ctime_nsec = 0;
            Ok(stat.assume_init())
        }
    }

    async fn symlink(&self, target: &Path, linkpath: &Path) -> VfsResult<()> {
        let linkpath_rel = self.translate_to_relative(linkpath)?;
        let target_str = target
            .to_str()
            .ok_or_else(|| VfsError::InvalidInput("Invalid target path".to_string()))?;

        let (parent_path, name) = Self::split_path(&linkpath_rel)?;
        let parent_ino = self.resolve_path(&parent_path).await?;

        self.fs
            .symlink(parent_ino, &name, target_str, 0, 0)
            .await
            .map_err(|e| {
                let err_msg = e.to_string();
                if err_msg.contains("already exists") {
                    VfsError::AlreadyExists
                } else {
                    VfsError::Other(format!("Failed to create symlink: {}", e))
                }
            })?;

        Ok(())
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let relative_path = self.translate_to_relative(path)?;

        let ino = self.resolve_path(&relative_path).await?;
        let target = self
            .fs
            .readlink(ino)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to read symlink: {}", e)))?
            .ok_or(VfsError::NotFound)?;

        Ok(PathBuf::from(target))
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<()> {
        let oldpath_rel = self.translate_to_relative(oldpath)?;
        let newpath_rel = self.translate_to_relative(newpath)?;

        let old_ino = self.resolve_path(&oldpath_rel).await?;
        let (new_parent_path, new_name) = Self::split_path(&newpath_rel)?;
        let new_parent_ino = self.resolve_path(&new_parent_path).await?;

        self.fs
            .link(old_ino, new_parent_ino, &new_name)
            .await
            .map_err(|e| {
                let err_msg = e.to_string();
                if err_msg.contains("does not exist") {
                    VfsError::NotFound
                } else if err_msg.contains("already exists") {
                    VfsError::AlreadyExists
                } else if err_msg.contains("directory") {
                    VfsError::PermissionDenied
                } else {
                    VfsError::Other(format!("Failed to create hard link: {}", e))
                }
            })?;

        Ok(())
    }
}

/// File operations for SQLite VFS files
struct SqliteFileOps {
    fs: Arc<dyn FileSystem>,
    ino: i64,
    path: String,
    data: Arc<Mutex<Vec<u8>>>,
    offset: Arc<Mutex<i64>>,
    flags: Mutex<i32>,
    dirty: Arc<Mutex<bool>>,
}

impl SqliteFileOps {
    /// Resolve the path and get the inode (for new files)
    async fn get_or_create_ino(&self) -> VfsResult<i64> {
        if self.ino != 0 {
            return Ok(self.ino);
        }

        // Need to create the file
        let (parent_path, name) = SqliteVfs::split_path(&self.path)?;

        // Walk to parent
        let mut parent_ino = ROOT_INO;
        for component in parent_path.split('/').filter(|s| !s.is_empty()) {
            let stats = self
                .fs
                .lookup(parent_ino, component)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to lookup: {}", e)))?
                .ok_or(VfsError::NotFound)?;
            parent_ino = stats.ino;
        }

        // Create the file
        let (stats, _file) = self
            .fs
            .create_file(parent_ino, &name, 0o644, 0, 0)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to create file: {}", e)))?;

        Ok(stats.ino)
    }
}

#[async_trait::async_trait]
impl FileOps for SqliteFileOps {
    async fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.data.lock().unwrap();
        let mut offset = self.offset.lock().unwrap();

        let start = *offset as usize;
        if start >= data.len() {
            return Ok(0);
        }

        let end = std::cmp::min(start + buf.len(), data.len());
        let bytes_read = end - start;
        buf[..bytes_read].copy_from_slice(&data[start..end]);
        *offset += bytes_read as i64;

        Ok(bytes_read)
    }

    async fn write(&self, buf: &[u8]) -> VfsResult<usize> {
        let mut data = self.data.lock().unwrap();
        let mut offset = self.offset.lock().unwrap();
        let flags = *self.flags.lock().unwrap();

        // Handle O_APPEND: always write at the end of the file
        let start = if flags & libc::O_APPEND != 0 {
            data.len()
        } else {
            *offset as usize
        };

        // Extend the buffer if necessary
        if start + buf.len() > data.len() {
            data.resize(start + buf.len(), 0);
        }

        data[start..start + buf.len()].copy_from_slice(buf);
        *offset = (start + buf.len()) as i64;

        // Mark as dirty since we modified the data
        *self.dirty.lock().unwrap() = true;

        Ok(buf.len())
    }

    async fn seek(&self, offset: i64, whence: i32) -> VfsResult<i64> {
        let data = self.data.lock().unwrap();
        let mut current_offset = self.offset.lock().unwrap();

        let new_offset = match whence {
            libc::SEEK_SET => offset,
            libc::SEEK_CUR => *current_offset + offset,
            libc::SEEK_END => data.len() as i64 + offset,
            _ => return Err(VfsError::Other("Invalid whence".to_string())),
        };

        if new_offset < 0 {
            return Err(VfsError::Other("Invalid offset".to_string()));
        }

        *current_offset = new_offset;
        Ok(new_offset)
    }

    async fn fstat(&self) -> VfsResult<libc::stat> {
        // Get the actual file stats from the filesystem
        let ino = self.get_or_create_ino().await?;
        let stats = self
            .fs
            .getattr(ino)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to getattr: {}", e)))?
            .ok_or(VfsError::NotFound)?;

        let data = self.data.lock().unwrap();

        // Use MaybeUninit to construct libc::stat safely
        let mut stat: std::mem::MaybeUninit<libc::stat> = std::mem::MaybeUninit::zeroed();
        unsafe {
            let stat_ptr = stat.as_mut_ptr();
            (*stat_ptr).st_dev = 0;
            (*stat_ptr).st_ino = stats.ino as u64;
            (*stat_ptr).st_nlink = stats.nlink.into();
            (*stat_ptr).st_mode = stats.mode;
            (*stat_ptr).st_uid = stats.uid;
            (*stat_ptr).st_gid = stats.gid;
            (*stat_ptr).st_rdev = 0;
            (*stat_ptr).st_size = data.len() as i64;
            (*stat_ptr).st_blksize = 4096;
            (*stat_ptr).st_blocks = (data.len() as i64 + 4095) / 4096;
            (*stat_ptr).st_atime = stats.atime;
            (*stat_ptr).st_atime_nsec = 0;
            (*stat_ptr).st_mtime = stats.mtime;
            (*stat_ptr).st_mtime_nsec = 0;
            (*stat_ptr).st_ctime = stats.ctime;
            (*stat_ptr).st_ctime_nsec = 0;
            Ok(stat.assume_init())
        }
    }

    async fn fsync(&self) -> VfsResult<()> {
        // For virtual file, sync means write to database
        let dirty = *self.dirty.lock().unwrap();
        if !dirty {
            return Ok(());
        }

        let data = self.data.lock().unwrap().clone();
        let ino = self.get_or_create_ino().await?;

        // Write the data to the database
        let file = self
            .fs
            .open(ino, libc::O_RDWR)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to open file: {}", e)))?;
        file.pwrite(0, &data)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to write file: {}", e)))?;
        file.truncate(data.len() as u64)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to truncate file: {}", e)))?;

        // Clear dirty flag after successful write
        *self.dirty.lock().unwrap() = false;

        Ok(())
    }

    async fn fdatasync(&self) -> VfsResult<()> {
        // For virtual file, same as fsync
        self.fsync().await
    }

    fn fcntl(&self, cmd: i32, arg: i64) -> VfsResult<i64> {
        match cmd {
            libc::F_GETFL => Ok(self.get_flags() as i64),
            libc::F_SETFL => {
                self.set_flags(arg as i32)?;
                Ok(0)
            }
            _ => Err(VfsError::Other(format!(
                "Unsupported fcntl command: {}",
                cmd
            ))),
        }
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> VfsResult<i64> {
        // Virtual file doesn't support ioctl
        Err(VfsError::Other("ioctl not supported".to_string()))
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        // No real kernel FD for virtual files
        None
    }

    async fn close(&self) -> VfsResult<()> {
        // Ensure all data is written to the database before closing
        self.fsync().await
    }

    fn get_flags(&self) -> i32 {
        *self.flags.lock().unwrap()
    }

    fn set_flags(&self, flags: i32) -> VfsResult<()> {
        *self.flags.lock().unwrap() = flags;
        Ok(())
    }
}

/// Type alias for directory entry list: (inode, name, type)
type DirEntryList = Vec<(u64, String, u8)>;

/// Directory operations for SQLite VFS directories
struct SqliteDirectoryOps {
    fs: Arc<dyn FileSystem>,
    ino: i64,
    path: String,
    flags: Mutex<i32>,
    /// Cached directory entries
    entries: Arc<Mutex<Option<DirEntryList>>>,
    /// Current position in the directory listing
    position: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl FileOps for SqliteDirectoryOps {
    async fn read(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        // Cannot read from a directory
        Err(VfsError::Other("Is a directory".to_string()))
    }

    async fn write(&self, _buf: &[u8]) -> VfsResult<usize> {
        // Cannot write to a directory
        Err(VfsError::Other("Is a directory".to_string()))
    }

    async fn seek(&self, _offset: i64, _whence: i32) -> VfsResult<i64> {
        // Cannot seek in a directory
        Err(VfsError::Other("Is a directory".to_string()))
    }

    async fn fstat(&self) -> VfsResult<libc::stat> {
        // Get stats from the filesystem
        let stats = self
            .fs
            .getattr(self.ino)
            .await
            .map_err(|e| VfsError::Other(format!("Failed to getattr: {}", e)))?
            .ok_or(VfsError::NotFound)?;

        // Use MaybeUninit to construct libc::stat safely
        let mut stat: std::mem::MaybeUninit<libc::stat> = std::mem::MaybeUninit::zeroed();
        unsafe {
            let stat_ptr = stat.as_mut_ptr();
            (*stat_ptr).st_dev = 0;
            (*stat_ptr).st_ino = stats.ino as u64;
            (*stat_ptr).st_nlink = stats.nlink.into();
            (*stat_ptr).st_mode = stats.mode;
            (*stat_ptr).st_uid = stats.uid;
            (*stat_ptr).st_gid = stats.gid;
            (*stat_ptr).st_rdev = 0;
            (*stat_ptr).st_size = stats.size;
            (*stat_ptr).st_blksize = 4096;
            (*stat_ptr).st_blocks = (stats.size + 4095) / 4096;
            (*stat_ptr).st_atime = stats.atime;
            (*stat_ptr).st_atime_nsec = 0;
            (*stat_ptr).st_mtime = stats.mtime;
            (*stat_ptr).st_mtime_nsec = 0;
            (*stat_ptr).st_ctime = stats.ctime;
            (*stat_ptr).st_ctime_nsec = 0;
            Ok(stat.assume_init())
        }
    }

    async fn fsync(&self) -> VfsResult<()> {
        // Nothing to sync for directories
        Ok(())
    }

    async fn fdatasync(&self) -> VfsResult<()> {
        // Nothing to sync for directories
        Ok(())
    }

    fn fcntl(&self, cmd: i32, arg: i64) -> VfsResult<i64> {
        match cmd {
            libc::F_GETFL => Ok(self.get_flags() as i64),
            libc::F_SETFL => {
                self.set_flags(arg as i32)?;
                Ok(0)
            }
            _ => Err(VfsError::Other(format!(
                "Unsupported fcntl command: {}",
                cmd
            ))),
        }
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> VfsResult<i64> {
        // Virtual directory doesn't support ioctl
        Err(VfsError::Other("ioctl not supported".to_string()))
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        // No real kernel FD for virtual directories
        None
    }

    async fn close(&self) -> VfsResult<()> {
        // Nothing to do when closing a directory
        Ok(())
    }

    fn get_flags(&self) -> i32 {
        *self.flags.lock().unwrap()
    }

    fn set_flags(&self, flags: i32) -> VfsResult<()> {
        *self.flags.lock().unwrap() = flags;
        Ok(())
    }

    async fn getdents(&self) -> VfsResult<DirEntryList> {
        // Check if we need to populate the entries cache
        let needs_populate = {
            let entries_lock = self.entries.lock().unwrap();
            entries_lock.is_none()
        };

        if needs_populate {
            // Read directory entries from the filesystem using readdir_plus
            let dir_entries = self
                .fs
                .readdir_plus(self.ino)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to read directory: {}", e)))?
                .ok_or(VfsError::NotFound)?;

            // Convert to the format expected by getdents64
            let mut result = Vec::new();

            // Get current directory stats for "."
            let current_stats = self
                .fs
                .getattr(self.ino)
                .await
                .map_err(|e| VfsError::Other(format!("Failed to getattr current dir: {}", e)))?
                .ok_or(VfsError::NotFound)?;

            // Get parent directory inode for ".."
            // Walk the path to find the parent
            let parent_ino = if self.path == "/" {
                ROOT_INO // Root's parent is itself
            } else {
                let parent_path = std::path::Path::new(&self.path)
                    .parent()
                    .map(|p| p.to_str().unwrap_or("/").to_string())
                    .unwrap_or("/".to_string());
                let parent_path = if parent_path.is_empty() {
                    "/"
                } else {
                    &parent_path
                };

                // Walk to find parent inode
                let mut ino = ROOT_INO;
                for component in parent_path.split('/').filter(|s| !s.is_empty()) {
                    if let Some(stats) = self
                        .fs
                        .lookup(ino, component)
                        .await
                        .map_err(|e| VfsError::Other(format!("Failed to lookup: {}", e)))?
                    {
                        ino = stats.ino;
                    }
                }
                ino
            };

            result.push((current_stats.ino as u64, ".".to_string(), libc::DT_DIR));
            result.push((parent_ino as u64, "..".to_string(), libc::DT_DIR));

            for entry in dir_entries {
                let d_type = if entry.stats.is_directory() {
                    libc::DT_DIR
                } else if entry.stats.is_symlink() {
                    libc::DT_LNK
                } else {
                    libc::DT_REG
                };
                result.push((entry.stats.ino as u64, entry.name, d_type));
            }

            // Store the results
            let mut entries_lock = self.entries.lock().unwrap();
            *entries_lock = Some(result);
        }

        // Get the current position and return entries
        let mut position = self.position.lock().unwrap();
        let entries_lock = self.entries.lock().unwrap();
        let all_entries = entries_lock.as_ref().unwrap();

        if *position >= all_entries.len() {
            // No more entries - return empty to signal EOF
            Ok(Vec::new())
        } else {
            // Return remaining entries and update position
            let remaining = all_entries[*position..].to_vec();
            *position = all_entries.len();
            Ok(remaining)
        }
    }
}
