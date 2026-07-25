use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use local_rpc::{
    bulk::BulkFinished,
    model::{AttachmentDescriptor, AttachmentId, BulkTransferId},
};

#[derive(Clone)]
pub struct CachedAttachment {
    id: AttachmentId,
    revision: u64,
    bytes: Arc<Vec<u8>>,
}

impl CachedAttachment {
    pub fn id(&self) -> AttachmentId {
        self.id
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl fmt::Debug for CachedAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedAttachment")
            .field("id", &self.id)
            .field("revision", &self.revision)
            .field("len", &self.len())
            .finish()
    }
}

impl Hash for CachedAttachment {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.revision.hash(state);
    }
}

struct PartialEntry {
    descriptor: AttachmentDescriptor,
    bytes: Vec<u8>,
}

struct CacheEntry {
    attachment: CachedAttachment,
    touched: u64,
}

pub struct MediaCache {
    partial: HashMap<BulkTransferId, PartialEntry>,
    entries: HashMap<AttachmentId, CacheEntry>,
    total_bytes: u64,
    partial_bytes: u64,
    budget_bytes: u64,
    clock: u64,
    revision: u64,
}

impl MediaCache {
    pub fn new(budget_bytes: u64) -> Self {
        Self {
            partial: HashMap::new(),
            entries: HashMap::new(),
            total_bytes: 0,
            partial_bytes: 0,
            budget_bytes,
            clock: 0,
            revision: 0,
        }
    }

    pub fn contains(&self, id: AttachmentId) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn get(&mut self, id: AttachmentId) -> Option<CachedAttachment> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&id)?;
        entry.touched = self.clock;
        Some(entry.attachment.clone())
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
        if descriptor.byte_len > self.budget_bytes {
            return Err("attachment exceeds the available media cache budget".into());
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
        self.partial.insert(
            transfer_id,
            PartialEntry {
                descriptor: descriptor.clone(),
                bytes: Vec::new(),
            },
        );
        self.partial_bytes = self.partial_bytes.saturating_add(descriptor.byte_len);
        log::info!(
            "attachment cache reserved bulk_transfer_id={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} bytes={}",
            transfer_id.0,
            descriptor.id.timestamp_ms,
            descriptor.id.transfer_id.0,
            descriptor.file_name,
            descriptor.byte_len,
        );
        Ok(())
    }

    pub fn chunk(&mut self, transfer_id: BulkTransferId, bytes: &[u8]) -> Result<(), String> {
        let validation = self
            .partial
            .get(&transfer_id)
            .ok_or_else(|| "bulk chunk has no active transfer".to_string())
            .and_then(|partial| {
                let received = u64::try_from(partial.bytes.len())
                    .map_err(|_| "attachment length cannot be represented".to_string())?;
                let incoming = u64::try_from(bytes.len())
                    .map_err(|_| "bulk chunk length cannot be represented".to_string())?;
                received
                    .checked_add(incoming)
                    .filter(|total| *total <= partial.descriptor.byte_len)
                    .ok_or_else(|| "bulk chunk exceeds declared attachment length".to_string())
            });
        if let Err(error) = validation {
            self.cancel(transfer_id);
            return Err(error);
        }

        let partial = self
            .partial
            .get_mut(&transfer_id)
            .expect("validated bulk transfer remains active");
        if let Err(error) = partial.bytes.try_reserve(bytes.len()) {
            let error = format!("cannot allocate attachment chunk: {error}");
            self.cancel(transfer_id);
            return Err(error);
        }
        partial.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn finish(&mut self, finished: BulkFinished) -> Result<AttachmentDescriptor, String> {
        let Some(partial) = self.partial.remove(&finished.transfer_id) else {
            return Err("bulk finish has no active transfer".into());
        };
        self.partial_bytes = self
            .partial_bytes
            .saturating_sub(partial.descriptor.byte_len);
        let actual_len = u64::try_from(partial.bytes.len())
            .map_err(|_| "attachment length cannot be represented".to_string())?;
        if actual_len != partial.descriptor.byte_len {
            return Err("attachment length verification failed".into());
        }
        let Some(revision) = self.revision.checked_add(1) else {
            return Err("attachment cache revision space exhausted".into());
        };
        self.revision = revision;
        self.clock = self.clock.wrapping_add(1);
        let attachment = CachedAttachment {
            id: partial.descriptor.id,
            revision,
            bytes: Arc::new(partial.bytes),
        };
        self.total_bytes = self.total_bytes.saturating_add(actual_len);
        self.entries.insert(
            partial.descriptor.id,
            CacheEntry {
                attachment,
                touched: self.clock,
            },
        );
        log::info!(
            "attachment cache finalized bulk_transfer_id={} attachment_timestamp_ms={} attachment_transfer_id={} file={:?} bytes={} revision={}",
            finished.transfer_id.0,
            partial.descriptor.id.timestamp_ms,
            partial.descriptor.id.transfer_id.0,
            partial.descriptor.file_name,
            actual_len,
            revision,
        );
        self.evict();
        Ok(partial.descriptor)
    }

    pub fn cancel(&mut self, transfer_id: BulkTransferId) {
        if let Some(partial) = self.partial.remove(&transfer_id) {
            self.partial_bytes = self
                .partial_bytes
                .saturating_sub(partial.descriptor.byte_len);
        }
    }

    pub fn cancel_all(&mut self) {
        self.partial.clear();
        self.partial_bytes = 0;
    }

    pub fn clear(&mut self) {
        self.cancel_all();
        self.entries.clear();
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
            self.remove_entry(key);
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
            self.remove_entry(key);
        }
    }

    fn remove_entry(&mut self, id: AttachmentId) {
        if let Some(entry) = self.entries.remove(&id) {
            self.total_bytes = self
                .total_bytes
                .saturating_sub(entry.attachment.len() as u64);
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

    fn finish(cache: &mut MediaCache, transfer_id: u64) {
        cache
            .finish(BulkFinished {
                transfer_id: BulkTransferId(transfer_id),
            })
            .unwrap();
    }

    #[test]
    fn rejects_oversized_chunks_and_drops_partial_bytes() {
        let mut cache = MediaCache::new(1024);
        let descriptor = descriptor(1, 1, 4);
        cache.reserve(BulkTransferId(1), &descriptor).unwrap();
        assert_eq!(cache.partial_bytes, 4);
        assert!(cache.chunk(BulkTransferId(1), b"hello").is_err());
        assert!(cache.partial.is_empty());
        assert_eq!(cache.partial_bytes, 0);
    }

    #[test]
    fn rejects_unrequested_and_duplicate_attachment_transfers() {
        let descriptor = descriptor(9, 9, 0);
        let mut cache = MediaCache::new(1024);
        assert!(cache.chunk(BulkTransferId(9), &[1]).is_err());
        cache.reserve(BulkTransferId(9), &descriptor).unwrap();
        assert!(cache.reserve(BulkTransferId(10), &descriptor).is_err());
        finish(&mut cache, 9);
        assert!(cache.reserve(BulkTransferId(11), &descriptor).is_err());
    }

    #[test]
    fn reports_remaining_attachment_read_capacity() {
        let mut cache = MediaCache::new(1024);
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
    fn finishes_into_immutable_memory_bytes() {
        let bytes = b"cached media";
        let descriptor = descriptor(2, 3, bytes.len() as u64);
        let mut cache = MediaCache::new(1024);
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache.chunk(BulkTransferId(2), bytes).unwrap();
        finish(&mut cache, 2);

        assert!(cache.contains(descriptor.id));
        let attachment = cache.get(descriptor.id).unwrap();
        assert_eq!(attachment.id(), descriptor.id);
        assert_eq!(attachment.len(), bytes.len());
        assert!(!attachment.is_empty());
        assert_eq!(attachment.bytes(), bytes);
        assert!(!format!("{attachment:?}").contains("cached media"));
    }

    #[test]
    fn same_filename_transfers_keep_independent_bytes() {
        let first = descriptor(1_000, 7, 3);
        let second = descriptor(2_000, 8, 4);
        assert_eq!(first.file_name, second.file_name);
        let mut cache = MediaCache::new(1024);

        cache.reserve(BulkTransferId(1), &first).unwrap();
        cache.chunk(BulkTransferId(1), b"one").unwrap();
        finish(&mut cache, 1);
        cache.reserve(BulkTransferId(2), &second).unwrap();
        cache.chunk(BulkTransferId(2), b"two!").unwrap();
        finish(&mut cache, 2);

        assert_eq!(cache.get(first.id).unwrap().bytes(), b"one");
        assert_eq!(cache.get(second.id).unwrap().bytes(), b"two!");
    }

    #[test]
    fn rejects_non_contiguous_length_at_finish() {
        let descriptor = descriptor(2, 3, 4);
        let mut cache = MediaCache::new(1024);
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache.chunk(BulkTransferId(2), b"abc").unwrap();
        assert!(
            cache
                .finish(BulkFinished {
                    transfer_id: BulkTransferId(2),
                })
                .is_err()
        );
        assert!(!cache.contains(descriptor.id));
        assert_eq!(cache.partial_bytes, 0);
    }

    #[test]
    fn cancel_releases_declared_partial_budget() {
        let active = descriptor(2, 3, 800);
        let mut cache = MediaCache::new(1024);
        cache.reserve(BulkTransferId(2), &active).unwrap();
        cache.chunk(BulkTransferId(2), b"abc").unwrap();
        cache.cancel(BulkTransferId(2));
        assert_eq!(cache.partial_bytes, 0);
        assert!(cache.partial.is_empty());
        cache
            .reserve(BulkTransferId(3), &descriptor(3, 4, 1024))
            .unwrap();
    }

    #[test]
    fn clear_drops_completed_entries_and_resets_accounting() {
        let descriptor = descriptor(2, 3, 3);
        let mut cache = MediaCache::new(1024);
        cache.reserve(BulkTransferId(2), &descriptor).unwrap();
        cache.chunk(BulkTransferId(2), b"abc").unwrap();
        finish(&mut cache, 2);
        let first_revision = cache.get(descriptor.id).unwrap().revision;
        cache.clear();
        assert!(!cache.contains(descriptor.id));
        assert_eq!(cache.total_bytes, 0);
        assert_eq!(cache.partial_bytes, 0);

        cache.reserve(BulkTransferId(3), &descriptor).unwrap();
        cache.chunk(BulkTransferId(3), b"def").unwrap();
        finish(&mut cache, 3);
        assert!(cache.get(descriptor.id).unwrap().revision > first_revision);
    }

    #[test]
    fn evicts_least_recently_used_completed_bytes() {
        let first = descriptor(1, 1, 4);
        let second = descriptor(2, 2, 4);
        let third = descriptor(3, 3, 4);
        let mut cache = MediaCache::new(8);
        for (transfer, descriptor, bytes) in [
            (1, &first, b"1111".as_slice()),
            (2, &second, b"2222".as_slice()),
        ] {
            cache.reserve(BulkTransferId(transfer), descriptor).unwrap();
            cache.chunk(BulkTransferId(transfer), bytes).unwrap();
            finish(&mut cache, transfer);
        }
        cache.get(first.id).unwrap();
        cache.reserve(BulkTransferId(3), &third).unwrap();
        cache.chunk(BulkTransferId(3), b"3333").unwrap();
        finish(&mut cache, 3);

        assert!(cache.contains(first.id));
        assert!(!cache.contains(second.id));
        assert!(cache.contains(third.id));
        assert_eq!(cache.total_bytes, 8);
    }

    #[test]
    fn rejects_attachment_larger_than_budget_before_receiving_chunks() {
        let descriptor = descriptor(1, 1, 1025);
        let mut cache = MediaCache::new(1024);
        assert!(cache.reserve(BulkTransferId(1), &descriptor).is_err());
        assert!(cache.partial.is_empty());
        assert_eq!(cache.partial_bytes, 0);
    }
}
