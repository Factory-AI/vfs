//! External-base mutation watcher for non-zero kernel metadata TTL grants.
//!
//! Recursive watches are installed before the FUSE mount becomes observable,
//! then a dedicated thread invalidates every tracked external-origin kernel
//! grant after host mutations. Read-path events are excluded from the inotify
//! mask so unchanged base reads do not recreate the callback storm this
//! watcher exists to avoid. Losing watch coverage or a kernel invalidation
//! unmounts the session rather than permitting a stale-cache window.

use super::cache::AdapterCaches;
use crate::transport::{Notifier, SessionUnmounter};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

const WATCH_MASK: WatchMask = WatchMask::ATTRIB
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::MOVE_SELF)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO);

pub(super) struct BaseWatcher {
    wake: UnixStream,
    thread: Option<JoinHandle<()>>,
}

pub(super) struct PreparedBaseWatcher {
    inotify: Inotify,
    watch_root: PathBuf,
    ignored_paths: Vec<PathBuf>,
    watched: HashMap<WatchDescriptor, PathBuf>,
}

impl Drop for BaseWatcher {
    fn drop(&mut self) {
        let _ = self.wake.write(&[1]);
        if let Some(thread) = self.thread.take() {
            if let Err(panic) = thread.join() {
                tracing::warn!(?panic, "external base watcher thread panicked");
            }
        }
    }
}

pub(super) fn prepare(
    watch_root: Option<PathBuf>,
    ignored_paths: Vec<PathBuf>,
) -> anyhow::Result<Option<PreparedBaseWatcher>> {
    let Some(watch_root) = watch_root else {
        return Ok(None);
    };

    let inotify = Inotify::init().map_err(|error| {
        anyhow::anyhow!(
            "failed to create external base watcher for {}: {error}",
            watch_root.display()
        )
    })?;
    let mut watched = HashMap::new();
    add_recursive_watches(&inotify, &watch_root, &mut watched).map_err(|error| {
        anyhow::anyhow!(
            "failed to watch external base {}: {error}",
            watch_root.display()
        )
    })?;

    Ok(Some(PreparedBaseWatcher {
        inotify,
        watch_root,
        ignored_paths,
        watched,
    }))
}

impl PreparedBaseWatcher {
    pub(super) fn start(
        self,
        caches: Arc<AdapterCaches>,
        notifier: Notifier,
        mut unmounter: SessionUnmounter,
    ) -> anyhow::Result<BaseWatcher> {
        let Self {
            mut inotify,
            watch_root,
            ignored_paths,
            mut watched,
        } = self;
        let (wake_read, wake_write) = UnixStream::pair()
            .map_err(|error| anyhow::anyhow!("failed to create watcher wake pipe: {error}"))?;
        let thread = thread::Builder::new()
            .name("vfs-base-watch".into())
            .spawn(move || {
                if let Err(error) = watch_loop(
                    &mut inotify,
                    &wake_read,
                    &watch_root,
                    &ignored_paths,
                    &caches,
                    &notifier,
                    &mut watched,
                ) {
                    tracing::error!(
                        %error,
                        root = %watch_root.display(),
                        "external base watcher failed; unmounting to preserve cache coherence"
                    );
                    if let Err(unmount_error) = unmounter.unmount() {
                        tracing::warn!(
                            %unmount_error,
                            "failed to unmount after external base watcher failure"
                        );
                    }
                }
            })
            .map_err(|error| anyhow::anyhow!("failed to start external base watcher: {error}"))?;

        Ok(BaseWatcher {
            wake: wake_write,
            thread: Some(thread),
        })
    }
}

fn watch_loop(
    inotify: &mut Inotify,
    wake: &UnixStream,
    watch_root: &Path,
    ignored_paths: &[PathBuf],
    caches: &AdapterCaches,
    notifier: &Notifier,
    watched: &mut HashMap<WatchDescriptor, PathBuf>,
) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    loop {
        let mut poll_fds = [
            libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = loop {
            let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
            if result >= 0 {
                break result;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        if ready == 0 {
            continue;
        }
        if (poll_fds[1].revents & libc::POLLIN) != 0 {
            return Ok(());
        }
        if (poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
            return Err(io::Error::other("inotify descriptor became unusable"));
        }
        if (poll_fds[0].revents & libc::POLLIN) == 0 {
            continue;
        }

        let events = inotify.read_events(&mut buffer)?;
        let mut invalidate = false;
        for event in events {
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                return Err(io::Error::other("inotify event queue overflowed"));
            }
            let Some(parent) = watched.get(&event.wd).cloned() else {
                continue;
            };
            let path = event
                .name
                .map(|name| parent.join(name))
                .unwrap_or_else(|| parent.clone());

            if event.mask.contains(EventMask::IGNORED) {
                watched.remove(&event.wd);
                if path == watch_root {
                    return Err(io::Error::other("external base root watch was removed"));
                }
                continue;
            }
            if event.mask.contains(EventMask::ISDIR)
                && event
                    .mask
                    .intersects(EventMask::DELETE | EventMask::MOVED_FROM)
            {
                remove_subtree_watches(inotify, watched, &path);
            }
            if event.mask.contains(EventMask::ISDIR)
                && event
                    .mask
                    .intersects(EventMask::CREATE | EventMask::MOVED_TO)
            {
                if let Err(error) = add_recursive_watches(inotify, &path, watched) {
                    if error.kind() != io::ErrorKind::NotFound {
                        return Err(error);
                    }
                }
            }
            invalidate |= !ignored_paths
                .iter()
                .any(|ignored| watch_path_is_ignored(&path, ignored));
        }
        if invalidate {
            invalidate_kernel_grants(watch_root, caches, notifier)?;
        }
    }
}

fn add_recursive_watches(
    inotify: &Inotify,
    root: &Path,
    watched: &mut HashMap<WatchDescriptor, PathBuf>,
) -> io::Result<()> {
    let descriptor = inotify.watches().add(root, WATCH_MASK)?;
    watched.insert(descriptor, root.to_path_buf());

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            add_recursive_watches(inotify, &entry.path(), watched)?;
        }
    }
    Ok(())
}

fn remove_subtree_watches(
    inotify: &Inotify,
    watched: &mut HashMap<WatchDescriptor, PathBuf>,
    root: &Path,
) {
    let descriptors = watched
        .iter()
        .filter_map(|(descriptor, path)| {
            (path == root || path.starts_with(root)).then_some(descriptor.clone())
        })
        .collect::<Vec<_>>();
    for descriptor in descriptors {
        let _ = inotify.watches().remove(descriptor.clone());
        watched.remove(&descriptor);
    }
}

fn invalidate_kernel_grants(
    watch_root: &Path,
    caches: &AdapterCaches,
    notifier: &Notifier,
) -> io::Result<()> {
    let invalidation = caches.invalidate_external_kernel_state();
    for (parent, name) in &invalidation.entries {
        notifier.inval_entry(*parent, OsStr::new(name))?;
    }
    for ino in &invalidation.inodes {
        notifier.inval_inode(*ino, -1, 0)?;
        crate::telemetry::record_base_fast_inode_invalidation();
    }
    if !invalidation.entries.is_empty() || !invalidation.inodes.is_empty() {
        tracing::debug!(
            root = %watch_root.display(),
            entries = invalidation.entries.len(),
            inodes = invalidation.inodes.len(),
            "invalidated kernel metadata after external base mutation"
        );
    }
    Ok(())
}

fn watch_path_is_ignored(path: &Path, ignored: &Path) -> bool {
    if path == ignored {
        return true;
    }
    let (Some(path_parent), Some(ignored_parent), Some(path_name), Some(ignored_name)) = (
        path.parent(),
        ignored.parent(),
        path.file_name().and_then(OsStr::to_str),
        ignored.file_name().and_then(OsStr::to_str),
    ) else {
        return false;
    };
    path_parent == ignored_parent
        && path_name
            .strip_prefix(ignored_name)
            .is_some_and(|suffix| suffix.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::{watch_path_is_ignored, WATCH_MASK};
    use inotify::WatchMask;
    use std::path::Path;

    #[test]
    fn mutation_mask_excludes_read_path_events() {
        assert!(!WATCH_MASK.contains(WatchMask::ACCESS));
        assert!(!WATCH_MASK.contains(WatchMask::OPEN));
        assert!(!WATCH_MASK.contains(WatchMask::CLOSE_NOWRITE));
    }

    #[test]
    fn ignores_database_and_sidecars_only() {
        let database = Path::new("/repo/.vfs/session.db");

        assert!(watch_path_is_ignored(database, database));
        assert!(watch_path_is_ignored(
            Path::new("/repo/.vfs/session.db-wal"),
            database
        ));
        assert!(watch_path_is_ignored(
            Path::new("/repo/.vfs/session.db-journal"),
            database
        ));
        assert!(!watch_path_is_ignored(
            Path::new("/repo/session.db-wal"),
            database
        ));
        assert!(!watch_path_is_ignored(
            Path::new("/repo/.vfs/session.db.backup"),
            database
        ));
    }
}
