use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use local_rpc::{
    bulk::{BulkChunk, BulkFinished},
    model::{AttachmentDescriptor, AttachmentId, BulkTransferId},
};

struct PartialEntry {
    descriptor: AttachmentDescriptor,
    file: File,
    path: PathBuf,
    received: u64,
}

struct CacheEntry {
    path: PathBuf,
    byte_len: u64,
    touched: u64,
}

pub struct MediaCache {
    root: tempfile::TempDir,
    partial: HashMap<BulkTransferId, PartialEntry>,
    entries: HashMap<AttachmentId, CacheEntry>,
    total_bytes: u64,
    partial_bytes: u64,
    budget_bytes: u64,
    clock: u64,
}

impl MediaCache {
    pub fn new(budget_bytes: u64) -> io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("chatt-gui-cache-")
            .tempdir()?;
        Ok(Self {
            root,
            partial: HashMap::new(),
            entries: HashMap::new(),
            total_bytes: 0,
            partial_bytes: 0,
            budget_bytes,
            clock: 0,
        })
    }

    pub fn path_for(&mut self, descriptor: &AttachmentDescriptor) -> Option<PathBuf> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&descriptor.id)?;
        entry.touched = self.clock;
        Some(entry.path.clone())
    }

    pub fn active_transfer(&self, descriptor: &AttachmentDescriptor) -> Option<BulkTransferId> {
        self.partial.iter().find_map(|(transfer_id, partial)| {
            (partial.descriptor.id == descriptor.id).then_some(*transfer_id)
        })
    }

    pub fn available_transfer_slots(&self) -> usize {
        local_rpc::MAX_CONCURRENT_TRANSFERS.saturating_sub(self.partial.len())
    }

    pub fn reserve(
        &mut self,
        transfer_id: BulkTransferId,
        descriptor: &AttachmentDescriptor,
    ) -> Result<(), String> {
        if self.partial.len() >= local_rpc::MAX_CONCURRENT_TRANSFERS {
            return Err("too many media transfers".into());
        }
        if self.partial.contains_key(&transfer_id) {
            return Err("bulk transfer id is already active".into());
        }
        if self.entries.contains_key(&descriptor.id) || self.active_transfer(descriptor).is_some() {
            return Err("attachment is already cached or requested".into());
        }
        self.make_room(descriptor.byte_len);
        if self
            .total_bytes
            .saturating_add(self.partial_bytes)
            .saturating_add(descriptor.byte_len)
            > self.budget_bytes
        {
            return Err("attachment exceeds the available media cache budget".into());
        }
        let path = self.root.path().join(format!("{}.part", transfer_id.0));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("cannot create cache partial: {error}"))?;
        self.partial.insert(
            transfer_id,
            PartialEntry {
                descriptor: descriptor.clone(),
                file,
                path: path.clone(),
                received: 0,
            },
        );
        self.partial_bytes = self.partial_bytes.saturating_add(descriptor.byte_len);
        log::info!(
            "attachment cache reserved bulk_transfer_id={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} bytes={} partial_path={:?}",
            transfer_id.0,
            descriptor.id.timestamp_ms,
            descriptor.id.transfer_id.0,
            descriptor.file_name,
            descriptor.byte_len,
            path,
        );
        Ok(())
    }

    pub fn chunk(&mut self, chunk: BulkChunk) -> Result<(), String> {
        let partial = self
            .partial
            .get_mut(&chunk.transfer_id)
            .ok_or_else(|| "bulk chunk has no active transfer".to_string())?;
        if partial.received.saturating_add(chunk.bytes.len() as u64) > partial.descriptor.byte_len {
            self.cancel(chunk.transfer_id);
            return Err("bulk chunk exceeds declared attachment length".into());
        }
        if let Err(error) = partial.file.write_all(&chunk.bytes) {
            let reason = error.to_string();
            self.cancel(chunk.transfer_id);
            return Err(reason);
        }
        partial.received += chunk.bytes.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self, finished: BulkFinished) -> Result<AttachmentDescriptor, String> {
        let Some(partial) = self.partial.remove(&finished.transfer_id) else {
            return Err("bulk finish has no active transfer".into());
        };
        self.partial_bytes = self
            .partial_bytes
            .saturating_sub(partial.descriptor.byte_len);
        if partial.received != partial.descriptor.byte_len {
            let _ = fs::remove_file(&partial.path);
            return Err("attachment length verification failed".into());
        }
        drop(partial.file);
        let final_path = self.root.path().join(format!(
            "{}-{}.cache",
            partial.descriptor.id.timestamp_ms, partial.descriptor.id.transfer_id.0
        ));
        if let Err(error) = fs::rename(&partial.path, &final_path) {
            let _ = fs::remove_file(&partial.path);
            return Err(error.to_string());
        }
        self.clock = self.clock.wrapping_add(1);
        self.total_bytes = self.total_bytes.saturating_add(partial.received);
        self.entries.insert(
            partial.descriptor.id,
            CacheEntry {
                path: final_path.clone(),
                byte_len: partial.received,
                touched: self.clock,
            },
        );
        log::info!(
            "attachment cache finalized bulk_transfer_id={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} bytes={} cache_path={:?}",
            finished.transfer_id.0,
            partial.descriptor.id.timestamp_ms,
            partial.descriptor.id.transfer_id.0,
            partial.descriptor.file_name,
            partial.received,
            final_path,
        );
        self.evict();
        Ok(partial.descriptor)
    }

    pub fn cancel(&mut self, transfer_id: BulkTransferId) {
        if let Some(partial) = self.partial.remove(&transfer_id) {
            self.partial_bytes = self
                .partial_bytes
                .saturating_sub(partial.descriptor.byte_len);
            drop(partial.file);
            let _ = fs::remove_file(partial.path);
        }
    }

    pub fn cancel_all(&mut self) {
        let transfers = self.partial.keys().copied().collect::<Vec<_>>();
        for transfer in transfers {
            self.cancel(transfer);
        }
    }

    pub fn clear(&mut self) {
        self.cancel_all();
        for (_, entry) in self.entries.drain() {
            let _ = fs::remove_file(entry.path);
        }
        self.total_bytes = 0;
    }

    fn evict(&mut self) {
        while self.total_bytes > self.budget_bytes {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.byte_len);
                let _ = fs::remove_file(entry.path);
            }
        }
    }

    fn make_room(&mut self, incoming: u64) {
        while self
            .total_bytes
            .saturating_add(self.partial_bytes)
            .saturating_add(incoming)
            > self.budget_bytes
        {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.byte_len);
                let _ = fs::remove_file(entry.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::{ids::FileTransferId, model::MediaKind};

    fn descriptor(timestamp_ms: u64, transfer: u64, byte_len: u64) -> AttachmentDescriptor {
        AttachmentDescriptor {
            id: AttachmentId {
                timestamp_ms,
                transfer_id: FileTransferId(transfer),
            },
            file_name: "../../escape.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len,
            width: None,
            height: None,
        }
    }

    #[test]
    fn rejects_oversized_chunks_and_cleans_partial() {
        let mut cache = MediaCache::new(1024).unwrap();
        let descriptor = descriptor(1, 1, 4);
        cache.reserve(BulkTransferId(1), &descriptor).unwrap();
        assert!(
            cache
                .chunk(BulkChunk {
                    transfer_id: BulkTransferId(1),
                    bytes: b"hello".to_vec(),
                })
                .is_err()
        );
        assert!(cache.partial.is_empty());
    }

    #[test]
    fn rejects_unrequested_and_duplicate_attachment_transfers() {
        let descriptor = descriptor(9, 9, 0);
        let mut cache = MediaCache::new(1024).unwrap();
        assert!(
            cache
                .chunk(BulkChunk {
                    transfer_id: BulkTransferId(9),
                    bytes: vec![1],
                })
                .is_err()
        );
        cache.reserve(BulkTransferId(9), &descriptor).unwrap();
        assert!(cache.reserve(BulkTransferId(10), &descriptor).is_err());
    }

    #[test]
    fn reports_remaining_attachment_read_capacity() {
        let mut cache = MediaCache::new(1024).unwrap();
        assert_eq!(
            cache.available_transfer_slots(),
            local_rpc::MAX_CONCURRENT_TRANSFERS
        );
        for index in 0..local_rpc::MAX_CONCURRENT_TRANSFERS {
            let descriptor = descriptor(1, (index + 1) as u64, 0);
            cache
                .reserve(BulkTransferId((index + 1) as u64), &descriptor)
                .unwrap();
        }
        assert_eq!(cache.available_transfer_slots(), 0);
    }

    #[test]
    fn verifies_length_and_never_uses_peer_filename_as_path() {
        let bytes = b"cached media";
        let descriptor = descriptor(2, 3, bytes.len() as u64);
        let mut cache = MediaCache::new(1024).unwrap();
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache
            .chunk(BulkChunk {
                transfer_id: BulkTransferId(2),
                bytes: bytes.to_vec(),
            })
            .unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(2),
            })
            .unwrap();
        let path = cache.path_for(&descriptor).unwrap();
        assert!(path.starts_with(cache.root.path()));
        assert!(!path.to_string_lossy().contains("escape"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn same_filename_uploads_keep_independent_cached_bytes() {
        let first = descriptor(1_000, 7, 3);
        let second = descriptor(2_000, 8, 4);
        assert_eq!(first.file_name, second.file_name);
        let mut cache = MediaCache::new(1024).unwrap();

        cache.reserve(BulkTransferId(1), &first).unwrap();
        cache
            .chunk(BulkChunk {
                transfer_id: BulkTransferId(1),
                bytes: b"one".to_vec(),
            })
            .unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(1),
            })
            .unwrap();

        cache.reserve(BulkTransferId(2), &second).unwrap();
        cache
            .chunk(BulkChunk {
                transfer_id: BulkTransferId(2),
                bytes: b"two!".to_vec(),
            })
            .unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(2),
            })
            .unwrap();

        let first_path = cache.path_for(&first).unwrap();
        let second_path = cache.path_for(&second).unwrap();
        assert_ne!(first_path, second_path);
        assert_eq!(fs::read(first_path).unwrap(), b"one");
        assert_eq!(fs::read(second_path).unwrap(), b"two!");
    }

    #[test]
    fn clear_removes_completed_entries() {
        let descriptor = descriptor(2, 3, 0);
        let mut cache = MediaCache::new(1024).unwrap();
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(2),
            })
            .unwrap();
        cache.clear();
        assert!(cache.path_for(&descriptor).is_none());
        assert_eq!(cache.total_bytes, 0);
    }
}
