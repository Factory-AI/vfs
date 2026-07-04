use super::{OverlayFS, ROOT_INO};
use crate::error::Result;
use crate::fs::FsError;
use std::collections::HashMap;

/// Which layer an inode belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Layer {
    Delta,
    Base,
}

/// Information about an inode in the overlay filesystem.
#[derive(Debug, Clone)]
pub(super) struct InodeInfo {
    /// Which layer this inode lives in.
    pub(super) layer: Layer,
    /// The inode number in the underlying layer.
    pub(super) underlying_ino: i64,
    /// Virtual path (for whiteout and copy-up operations).
    pub(super) path: String,
    /// Whether this inode also has an origin reverse-map entry to remove.
    has_extra_reverse: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OverlayMapCounts {
    pub(super) inode_entries: usize,
    pub(super) reverse_entries: usize,
    pub(super) path_entries: usize,
    pub(super) lookup_ref_entries: usize,
}

/// All overlay inode maps protected by a single mutex.
pub(super) struct OverlayMaps {
    inode: HashMap<i64, InodeInfo>,
    reverse: HashMap<(Layer, i64), i64>,
    path: HashMap<String, i64>,
    lookup_refs: HashMap<i64, u64>,
    next_ino: i64,
}

impl OverlayMaps {
    pub(super) fn new() -> Self {
        let mut inode = HashMap::new();
        let mut reverse = HashMap::new();
        let mut path = HashMap::new();

        inode.insert(
            ROOT_INO,
            InodeInfo {
                layer: Layer::Delta,
                underlying_ino: ROOT_INO,
                path: "/".to_string(),
                has_extra_reverse: false,
            },
        );
        reverse.insert((Layer::Delta, ROOT_INO), ROOT_INO);
        path.insert("/".to_string(), ROOT_INO);

        Self {
            inode,
            reverse,
            path,
            lookup_refs: HashMap::new(),
            next_ino: ROOT_INO + 1,
        }
    }

    fn alloc_ino(&mut self) -> i64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn get_or_create(&mut self, layer: Layer, underlying_ino: i64, path: &str) -> i64 {
        if let Some(&ino) = self.reverse.get(&(layer, underlying_ino)) {
            self.retain_lookup(ino, 1);
            return ino;
        }

        let ino = self.alloc_ino();
        self.inode.insert(
            ino,
            InodeInfo {
                layer,
                underlying_ino,
                path: path.to_string(),
                has_extra_reverse: false,
            },
        );
        self.reverse.insert((layer, underlying_ino), ino);
        self.path.insert(path.to_string(), ino);
        self.lookup_refs.insert(ino, 1);
        ino
    }

    fn refresh(
        &mut self,
        overlay_ino: i64,
        new_layer: Layer,
        new_underlying_ino: i64,
        new_path: &str,
    ) {
        let Some(info) = self.inode.get_mut(&overlay_ino) else {
            return;
        };
        let old_path = info.path.clone();
        let keeps_origin_reverse = info.layer == Layer::Base && new_layer == Layer::Delta;
        info.layer = new_layer;
        info.underlying_ino = new_underlying_ino;
        info.path = new_path.to_string();
        info.has_extra_reverse |= keeps_origin_reverse;

        self.reverse
            .insert((new_layer, new_underlying_ino), overlay_ino);
        if self.path.get(&old_path).copied() == Some(overlay_ino) {
            self.path.remove(&old_path);
        }
        self.path.insert(new_path.to_string(), overlay_ino);
    }

    fn promote_to_delta(&mut self, path: &str, delta_ino: i64) {
        let Some(&overlay_ino) = self.path.get(path) else {
            return;
        };
        let Some(info) = self.inode.get_mut(&overlay_ino) else {
            return;
        };
        if info.layer == Layer::Base {
            let old_base_ino = info.underlying_ino;
            info.layer = Layer::Delta;
            info.underlying_ino = delta_ino;
            info.has_extra_reverse = false;
            self.reverse.remove(&(Layer::Base, old_base_ino));
            self.reverse.insert((Layer::Delta, delta_ino), overlay_ino);
        }
    }

    fn retain_lookup(&mut self, ino: i64, nlookup: u64) {
        if ino == ROOT_INO || nlookup == 0 {
            return;
        }
        let refs = self.lookup_refs.entry(ino).or_insert(0);
        *refs = refs.saturating_add(nlookup);
    }

    fn forget_lookup(&mut self, ino: i64, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }

        let Some(refs) = self.lookup_refs.get_mut(&ino) else {
            return;
        };
        if *refs > nlookup {
            *refs -= nlookup;
            return;
        }

        self.lookup_refs.remove(&ino);
        self.prune_unreferenced(ino);
    }

    fn prune_unreferenced(&mut self, ino: i64) {
        let Some(info) = self.inode.remove(&ino) else {
            return;
        };
        if self.path.get(&info.path).copied() == Some(ino) {
            self.path.remove(&info.path);
        }
        self.reverse.remove(&(info.layer, info.underlying_ino));
        if info.has_extra_reverse {
            // Copied-up inodes may also keep a base-origin reverse entry for
            // same-session inode stability. Delta-only entries, the common
            // clone/write path, avoid the old whole-map scan.
            self.reverse.retain(|_, mapped_ino| *mapped_ino != ino);
        }
    }

    #[cfg(test)]
    fn counts(&self) -> OverlayMapCounts {
        OverlayMapCounts {
            inode_entries: self.inode.len(),
            reverse_entries: self.reverse.len(),
            path_entries: self.path.len(),
            lookup_ref_entries: self.lookup_refs.len(),
        }
    }
}

impl OverlayFS {
    /// Get or create an overlay inode for a layer inode.
    pub(super) fn get_or_create_overlay_ino(
        &self,
        layer: Layer,
        underlying_ino: i64,
        path: &str,
    ) -> i64 {
        self.maps.lock().get_or_create(layer, underlying_ino, path)
    }

    /// Refresh an existing overlay inode mapping to point at a new backing inode/path.
    pub(super) fn refresh_overlay_mapping(
        &self,
        overlay_ino: i64,
        new_layer: Layer,
        new_underlying_ino: i64,
        new_path: &str,
    ) {
        self.maps
            .lock()
            .refresh(overlay_ino, new_layer, new_underlying_ino, new_path);
    }

    /// Get inode info for an overlay inode.
    pub(super) fn get_inode_info(&self, ino: i64) -> Option<InodeInfo> {
        self.maps.lock().inode.get(&ino).cloned()
    }

    pub(super) fn live_origin_overlay_ino(&self, base_ino: i64, path: &str) -> Option<i64> {
        let maps = self.maps.lock();
        let overlay_ino = maps.reverse.get(&(Layer::Base, base_ino)).copied()?;
        let info = maps.inode.get(&overlay_ino)?;
        (info.path == path).then_some(overlay_ino)
    }

    /// Build path from parent inode and name.
    pub(super) fn build_path(&self, parent_ino: i64, name: &str) -> Result<String> {
        let info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        Ok(if info.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", info.path, name)
        })
    }

    /// Store origin mapping for copy-up
    pub(super) async fn add_origin_mapping(&self, delta_ino: i64, base_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)",
            (delta_ino, base_ino),
        )
        .await?;
        self.origin_map.write().insert(delta_ino, base_ino);
        Ok(())
    }

    /// Get origin inode for a delta inode
    pub(super) fn get_origin_ino(&self, delta_ino: i64) -> Option<i64> {
        self.origin_map.read().get(&delta_ino).copied()
    }

    pub(super) fn promote_mapping_to_delta(&self, path: &str, delta_ino: i64) {
        self.maps.lock().promote_to_delta(path, delta_ino);
    }

    pub(super) fn retain_overlay_lookup(&self, ino: i64, nlookup: u64) {
        self.maps.lock().retain_lookup(ino, nlookup);
    }

    pub(super) fn forget_overlay_lookup(&self, ino: i64, nlookup: u64) {
        self.maps.lock().forget_lookup(ino, nlookup);
    }

    #[cfg(test)]
    pub(super) fn debug_map_counts(&self) -> OverlayMapCounts {
        self.maps.lock().counts()
    }
}
