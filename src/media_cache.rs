use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use aws_lc_rs::digest::{Context, SHA256};
use rpc::daemon::{
    bulk::{BulkChunk, BulkFinished, BulkStarted},
    model::{AttachmentDescriptor, AttachmentId, BulkTransferId},
};

struct PartialEntry {
    descriptor: AttachmentDescriptor,
    file: File,
    path: PathBuf,
    offset: u64,
    digest: Context,
}

struct CacheEntry {
    path: PathBuf,
    byte_len: u64,
    touched: u64,
}

pub struct MediaCache {
    root: tempfile::TempDir,
    requested: HashMap<BulkTransferId, (AttachmentId, [u8; 32])>,
    partial: HashMap<BulkTransferId, PartialEntry>,
    entries: HashMap<(AttachmentId, [u8; 32]), CacheEntry>,
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
            requested: HashMap::new(),
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
        let entry = self.entries.get_mut(&(descriptor.id, descriptor.digest))?;
        entry.touched = self.clock;
        Some(entry.path.clone())
    }

    pub fn active_transfer(&self, descriptor: &AttachmentDescriptor) -> Option<BulkTransferId> {
        self.requested
            .iter()
            .find_map(|(transfer_id, (id, digest))| {
                (*id == descriptor.id && *digest == descriptor.digest).then_some(*transfer_id)
            })
            .or_else(|| {
                self.partial.iter().find_map(|(transfer_id, partial)| {
                    (partial.descriptor.id == descriptor.id
                        && partial.descriptor.digest == descriptor.digest)
                        .then_some(*transfer_id)
                })
            })
    }

    pub fn available_transfer_slots(&self) -> usize {
        rpc::daemon::MAX_CONCURRENT_TRANSFERS
            .saturating_sub(self.requested.len() + self.partial.len())
    }

    pub fn reserve(
        &mut self,
        transfer_id: BulkTransferId,
        descriptor: &AttachmentDescriptor,
    ) -> Result<(), String> {
        if self.requested.len() + self.partial.len() >= rpc::daemon::MAX_CONCURRENT_TRANSFERS {
            return Err("too many media transfers".into());
        }
        if self.requested.contains_key(&transfer_id) || self.partial.contains_key(&transfer_id) {
            return Err("bulk transfer id is already active".into());
        }
        if self
            .entries
            .contains_key(&(descriptor.id, descriptor.digest))
            || self.active_transfer(descriptor).is_some()
        {
            return Err("attachment is already cached or requested".into());
        }
        self.requested
            .insert(transfer_id, (descriptor.id, descriptor.digest));
        Ok(())
    }

    pub fn begin(&mut self, started: BulkStarted) -> Result<(), String> {
        let descriptor = started.attachment;
        let byte_len = descriptor.byte_len;
        let Some((expected_id, expected_digest)) = self.requested.remove(&started.transfer_id)
        else {
            return Err("attachment transfer was not requested".into());
        };
        if expected_id != descriptor.id || expected_digest != descriptor.digest {
            return Err("attachment transfer does not match its request".into());
        }
        if self.partial.len() >= rpc::daemon::MAX_CONCURRENT_TRANSFERS {
            return Err("too many media transfers".into());
        }
        if self.partial.contains_key(&started.transfer_id) {
            return Err("bulk transfer id is already active".into());
        }
        if self
            .entries
            .contains_key(&(descriptor.id, descriptor.digest))
        {
            return Err("attachment is already cached".into());
        }
        if self.partial.values().any(|partial| {
            partial.descriptor.id == descriptor.id && partial.descriptor.digest == descriptor.digest
        }) {
            return Err("attachment transfer is already active".into());
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
        let path = self.root.path().join(format!(
            "{}-{}.part",
            hex_id(descriptor.id),
            started.transfer_id.0
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("cannot create cache partial: {error}"))?;
        self.partial.insert(
            started.transfer_id,
            PartialEntry {
                descriptor,
                file,
                path,
                offset: 0,
                digest: Context::new(&SHA256),
            },
        );
        self.partial_bytes = self.partial_bytes.saturating_add(byte_len);
        Ok(())
    }

    pub fn chunk(&mut self, chunk: BulkChunk) -> Result<(), String> {
        let partial = self
            .partial
            .get_mut(&chunk.transfer_id)
            .ok_or_else(|| "bulk chunk has no active transfer".to_string())?;
        if chunk.offset != partial.offset {
            self.cancel(chunk.transfer_id);
            return Err("bulk chunk offset is not contiguous".into());
        }
        if partial.offset.saturating_add(chunk.bytes.len() as u64) > partial.descriptor.byte_len {
            self.cancel(chunk.transfer_id);
            return Err("bulk chunk exceeds declared attachment length".into());
        }
        if let Err(error) = partial.file.write_all(&chunk.bytes) {
            let reason = error.to_string();
            self.cancel(chunk.transfer_id);
            return Err(reason);
        }
        partial.digest.update(&chunk.bytes);
        partial.offset += chunk.bytes.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self, finished: BulkFinished) -> Result<AttachmentDescriptor, String> {
        let Some(mut partial) = self.partial.remove(&finished.transfer_id) else {
            return Err("bulk finish has no active transfer".into());
        };
        self.partial_bytes = self
            .partial_bytes
            .saturating_sub(partial.descriptor.byte_len);
        let actual_digest = partial.digest.finish();
        if partial.offset != finished.byte_len
            || partial.offset != partial.descriptor.byte_len
            || actual_digest.as_ref() != finished.digest
            || finished.digest != partial.descriptor.digest
        {
            let _ = fs::remove_file(&partial.path);
            return Err("attachment length or digest verification failed".into());
        }
        if let Err(error) = partial.file.flush().and_then(|_| partial.file.sync_all()) {
            let _ = fs::remove_file(&partial.path);
            return Err(error.to_string());
        }
        drop(partial.file);
        let final_path = self.root.path().join(format!(
            "{}-{}.cache",
            hex_id(partial.descriptor.id),
            hex_digest_prefix(partial.descriptor.digest)
        ));
        if let Err(error) = fs::rename(&partial.path, &final_path) {
            let _ = fs::remove_file(&partial.path);
            return Err(error.to_string());
        }
        self.clock = self.clock.wrapping_add(1);
        self.total_bytes = self.total_bytes.saturating_add(partial.offset);
        self.entries.insert(
            (partial.descriptor.id, partial.descriptor.digest),
            CacheEntry {
                path: final_path,
                byte_len: partial.offset,
                touched: self.clock,
            },
        );
        self.evict();
        Ok(partial.descriptor)
    }

    pub fn cancel(&mut self, transfer_id: BulkTransferId) {
        self.requested.remove(&transfer_id);
        if let Some(partial) = self.partial.remove(&transfer_id) {
            self.partial_bytes = self
                .partial_bytes
                .saturating_sub(partial.descriptor.byte_len);
            drop(partial.file);
            let _ = fs::remove_file(partial.path);
        }
    }

    pub fn cancel_all(&mut self) {
        self.requested.clear();
        let transfers = self.partial.keys().copied().collect::<Vec<_>>();
        for transfer in transfers {
            self.cancel(transfer);
        }
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

fn hex_id(id: AttachmentId) -> String {
    id.0.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn hex_digest_prefix(digest: [u8; 32]) -> String {
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::daemon::model::MediaKind;

    #[test]
    fn rejects_non_contiguous_chunks_and_cleans_partial() {
        let bytes = b"hello";
        let digest = aws_lc_rs::digest::digest(&SHA256, bytes);
        let mut digest_bytes = [0; 32];
        digest_bytes.copy_from_slice(digest.as_ref());
        let descriptor = AttachmentDescriptor {
            id: AttachmentId([1; 16]),
            file_name: "peer/name.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: 5,
            digest: digest_bytes,
            width: None,
            height: None,
        };
        let mut cache = MediaCache::new(1024).unwrap();
        cache.reserve(BulkTransferId(1), &descriptor).unwrap();
        cache
            .begin(BulkStarted {
                transfer_id: BulkTransferId(1),
                attachment: descriptor,
            })
            .unwrap();
        assert!(
            cache
                .chunk(BulkChunk {
                    transfer_id: BulkTransferId(1),
                    offset: 1,
                    bytes: bytes.to_vec()
                })
                .is_err()
        );
        assert!(cache.partial.is_empty());
    }

    #[test]
    fn rejects_unrequested_and_duplicate_attachment_transfers() {
        let descriptor = AttachmentDescriptor {
            id: AttachmentId([9; 16]),
            file_name: "photo.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: 0,
            digest: [0; 32],
            width: None,
            height: None,
        };
        let mut cache = MediaCache::new(1024).unwrap();
        assert!(
            cache
                .begin(BulkStarted {
                    transfer_id: BulkTransferId(9),
                    attachment: descriptor.clone(),
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
            rpc::daemon::MAX_CONCURRENT_TRANSFERS
        );
        for index in 0..rpc::daemon::MAX_CONCURRENT_TRANSFERS {
            let marker = (index + 1) as u8;
            let descriptor = AttachmentDescriptor {
                id: AttachmentId([marker; 16]),
                file_name: format!("photo-{index}.png"),
                media_kind: MediaKind::Image,
                content_type: "image/png".into(),
                byte_len: 0,
                digest: [marker; 32],
                width: Some(320),
                height: Some(240),
            };
            cache
                .reserve(BulkTransferId((index + 1) as u64), &descriptor)
                .unwrap();
        }
        assert_eq!(cache.available_transfer_slots(), 0);
    }

    #[test]
    fn verifies_digest_and_never_uses_peer_filename_as_path() {
        let bytes = b"verified media";
        let digest = aws_lc_rs::digest::digest(&SHA256, bytes);
        let mut digest_bytes = [0; 32];
        digest_bytes.copy_from_slice(digest.as_ref());
        let descriptor = AttachmentDescriptor {
            id: AttachmentId([2; 16]),
            file_name: "../../escape.png".into(),
            media_kind: MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: bytes.len() as u64,
            digest: digest_bytes,
            width: None,
            height: None,
        };
        let mut cache = MediaCache::new(1024).unwrap();
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache
            .begin(BulkStarted {
                transfer_id: BulkTransferId(2),
                attachment: descriptor.clone(),
            })
            .unwrap();
        cache
            .chunk(BulkChunk {
                transfer_id: BulkTransferId(2),
                offset: 0,
                bytes: bytes.to_vec(),
            })
            .unwrap();
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(2),
                byte_len: bytes.len() as u64,
                digest: digest_bytes,
            })
            .unwrap();
        let path = cache.path_for(&descriptor).unwrap();
        assert!(path.starts_with(cache.root.path()));
        assert!(!path.to_string_lossy().contains("escape"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
