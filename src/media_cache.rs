use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use aws_lc_rs::digest::{Context, SHA256};
use rpc::daemon::{
    bulk::{BulkChunk, BulkFinished, BulkObject, BulkStarted},
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
    partial: HashMap<BulkTransferId, PartialEntry>,
    entries: HashMap<(AttachmentId, [u8; 32]), CacheEntry>,
    total_bytes: u64,
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
        self.partial.iter().find_map(|(transfer_id, partial)| {
            (partial.descriptor.id == descriptor.id
                && partial.descriptor.digest == descriptor.digest)
                .then_some(*transfer_id)
        })
    }

    pub fn begin(&mut self, started: BulkStarted) -> Result<(), String> {
        let BulkObject::Attachment(descriptor) = started.object else {
            return Err("bulk object is not an attachment".into());
        };
        if descriptor.byte_len != started.byte_len || descriptor.digest != started.digest {
            return Err("attachment metadata changed at transfer start".into());
        }
        if self.partial.len() >= rpc::daemon::MAX_CONCURRENT_TRANSFERS {
            return Err("too many media transfers".into());
        }
        let path = self
            .root
            .path()
            .join(format!("{}.part", hex_id(descriptor.id)));
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
        partial
            .file
            .write_all(&chunk.bytes)
            .map_err(|error| error.to_string())?;
        partial.digest.update(&chunk.bytes);
        partial.offset += chunk.bytes.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self, finished: BulkFinished) -> Result<AttachmentDescriptor, String> {
        let Some(mut partial) = self.partial.remove(&finished.transfer_id) else {
            return Err("bulk finish has no active transfer".into());
        };
        let actual_digest = partial.digest.finish();
        if partial.offset != finished.byte_len
            || partial.offset != partial.descriptor.byte_len
            || actual_digest.as_ref() != finished.digest
            || finished.digest != partial.descriptor.digest
        {
            let _ = fs::remove_file(&partial.path);
            return Err("attachment length or digest verification failed".into());
        }
        partial.file.flush().map_err(|error| error.to_string())?;
        partial.file.sync_all().map_err(|error| error.to_string())?;
        drop(partial.file);
        let final_path = self.root.path().join(format!(
            "{}-{}.cache",
            hex_id(partial.descriptor.id),
            hex_digest_prefix(partial.descriptor.digest)
        ));
        fs::rename(&partial.path, &final_path).map_err(|error| error.to_string())?;
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
        if let Some(partial) = self.partial.remove(&transfer_id) {
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
    use rpc::daemon::{bulk::BulkTransport, model::MediaKind};

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
        cache
            .begin(BulkStarted {
                transfer_id: BulkTransferId(1),
                object: BulkObject::Attachment(descriptor),
                byte_len: 5,
                digest: digest_bytes,
                transport: BulkTransport::RpcChunksV1,
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
        cache
            .begin(BulkStarted {
                transfer_id: BulkTransferId(2),
                object: BulkObject::Attachment(descriptor.clone()),
                byte_len: bytes.len() as u64,
                digest: digest_bytes,
                transport: BulkTransport::RpcChunksV1,
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
