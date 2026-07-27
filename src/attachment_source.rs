use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ffi::c_char,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{FileExt, FileTypeExt},
            net::UnixStream,
        },
    },
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use libmpv2::Mpv;
use local_rpc::{
    attachment_stream::{ReadRequest, ResponseStatus, read_response, write_request},
    frame::AttachmentSourceTransport,
    ids::RoomId,
    model::AttachmentId,
    model::{AttachmentDescriptor, RequestId},
};

const BLOCK_BYTES: usize = 256 * 1024;
const BLOCK_CACHE_ENTRIES: usize = 8;
const STARTUP_READ_TRACE_LIMIT: u64 = 8;
const MPV_PROTOCOL: &str = "chatt-media";
const MPV_SCHEME: &str = "chatt-media://";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AttachmentSourceKey {
    pub namespace: u64,
    pub room_id: RoomId,
    pub attachment_id: AttachmentId,
}

pub(crate) struct AttachmentSource {
    key: AttachmentSourceKey,
    byte_len: u64,
    backend: AttachmentSourceBackend,
    failed: AtomicBool,
}

enum AttachmentSourceBackend {
    Direct(Arc<File>),
    Remote(RemoteReadAtSource),
}

impl AttachmentSource {
    pub(crate) fn from_descriptor(
        key: AttachmentSourceKey,
        byte_len: u64,
        transport: AttachmentSourceTransport,
        fd: OwnedFd,
        maximum_read_bytes: u32,
    ) -> Result<Arc<Self>> {
        if byte_len == 0 {
            bail!("attachment source has an invalid zero length");
        }
        i64::try_from(byte_len).context("attachment source is too large for libmpv")?;
        let file = File::from(fd);
        let metadata = file
            .metadata()
            .context("inspect attachment source descriptor")?;
        let backend = match transport {
            AttachmentSourceTransport::DirectFile => {
                if !metadata.file_type().is_file() {
                    bail!("direct attachment source descriptor is not a regular file");
                }
                if metadata.len() != byte_len {
                    bail!("direct attachment source length does not match its descriptor");
                }
                let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                if flags < 0 {
                    return Err(io::Error::last_os_error())
                        .context("inspect attachment source access mode");
                }
                if flags & libc::O_ACCMODE != libc::O_RDONLY {
                    bail!("direct attachment source descriptor is not read-only");
                }
                AttachmentSourceBackend::Direct(Arc::new(file))
            }
            AttachmentSourceTransport::ReadAtSocket => {
                if !metadata.file_type().is_socket() {
                    bail!("remote attachment source descriptor is not a Unix socket");
                }
                let maximum_read_bytes = usize::try_from(maximum_read_bytes)
                    .context("attachment read limit does not fit this platform")?;
                if maximum_read_bytes == 0
                    || maximum_read_bytes > local_rpc::MAX_ATTACHMENT_READ_BYTES
                {
                    bail!("attachment read limit is invalid");
                }
                let fd: OwnedFd = file.into();
                AttachmentSourceBackend::Remote(RemoteReadAtSource::new(
                    UnixStream::from(fd),
                    byte_len,
                    maximum_read_bytes,
                ))
            }
        };
        Ok(Arc::new(Self {
            key,
            byte_len,
            backend,
            failed: AtomicBool::new(false),
        }))
    }

    #[cfg(test)]
    pub(crate) fn direct(key: AttachmentSourceKey, file: File, byte_len: u64) -> Arc<Self> {
        Arc::new(Self {
            key,
            byte_len,
            backend: AttachmentSourceBackend::Direct(Arc::new(file)),
            failed: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn remote(
        key: AttachmentSourceKey,
        socket: UnixStream,
        byte_len: u64,
        maximum_read_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            byte_len,
            backend: AttachmentSourceBackend::Remote(RemoteReadAtSource::new(
                socket,
                byte_len,
                maximum_read_bytes,
            )),
            failed: AtomicBool::new(false),
        })
    }

    pub(crate) fn key(&self) -> AttachmentSourceKey {
        self.key
    }

    pub(crate) fn byte_len(&self) -> u64 {
        self.byte_len
    }

    pub(crate) fn is_remote(&self) -> bool {
        matches!(self.backend, AttachmentSourceBackend::Remote(_))
    }

    pub(crate) fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub(crate) fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() || offset >= self.byte_len {
            return Ok(0);
        }
        let remaining = self.byte_len - offset;
        let requested = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let output = &mut output[..requested];
        let result = match &self.backend {
            AttachmentSourceBackend::Direct(file) => (|| {
                let mut total = 0usize;
                while total < output.len() {
                    let read_offset = offset
                        .checked_add(total as u64)
                        .ok_or_else(|| anyhow!("attachment read offset overflow"))?;
                    let read = file
                        .read_at(&mut output[total..], read_offset)
                        .context("read direct attachment source")?;
                    if read == 0 {
                        bail!("attachment source ended before its advertised length");
                    }
                    total = total
                        .checked_add(read)
                        .ok_or_else(|| anyhow!("attachment read length overflow"))?;
                }
                Ok(total)
            })(),
            AttachmentSourceBackend::Remote(source) => source.read_at(offset, output),
        };
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
}

struct RemoteReadAtSource {
    state: Mutex<RemoteReadState>,
}

struct RemoteReadState {
    socket: UnixStream,
    byte_len: u64,
    maximum_read_bytes: usize,
    blocks: Vec<CachedBlock>,
    clock: u64,
}

struct CachedBlock {
    index: u64,
    bytes: Vec<u8>,
    valid_len: usize,
    touched: u64,
}

impl RemoteReadAtSource {
    fn new(socket: UnixStream, byte_len: u64, maximum_read_bytes: usize) -> Self {
        Self {
            state: Mutex::new(RemoteReadState {
                socket,
                byte_len,
                maximum_read_bytes,
                blocks: Vec::new(),
                clock: 0,
            }),
        }
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("attachment source lock is poisoned"))?;
        if offset >= state.byte_len {
            return Ok(0);
        }
        let remaining = state.byte_len - offset;
        let wanted = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let mut copied = 0usize;
        while copied < wanted {
            let absolute = offset
                .checked_add(copied as u64)
                .ok_or_else(|| anyhow!("attachment read offset overflow"))?;
            let block_index = absolute / BLOCK_BYTES as u64;
            let in_block = usize::try_from(absolute % BLOCK_BYTES as u64)
                .expect("block offset is bounded by usize block size");
            let block = state.block(block_index)?;
            if in_block >= block.valid_len {
                bail!("attachment source ended before its advertised length");
            }
            let available = block.valid_len - in_block;
            let take = available.min(wanted - copied);
            output[copied..copied + take].copy_from_slice(&block.bytes[in_block..in_block + take]);
            copied += take;
        }
        Ok(copied)
    }
}

impl RemoteReadState {
    fn block(&mut self, index: u64) -> Result<&CachedBlock> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(position) = self.blocks.iter().position(|block| block.index == index) {
            self.blocks[position].touched = self.clock;
            return Ok(&self.blocks[position]);
        }

        let offset = index
            .checked_mul(BLOCK_BYTES as u64)
            .ok_or_else(|| anyhow!("attachment block offset overflow"))?;
        if offset >= self.byte_len {
            bail!("attachment block begins after EOF");
        }
        let valid_len = usize::try_from((self.byte_len - offset).min(BLOCK_BYTES as u64))
            .context("attachment block length does not fit this platform")?;
        let mut bytes = Vec::with_capacity(valid_len);
        while bytes.len() < valid_len {
            let request_offset = offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| anyhow!("attachment request offset overflow"))?;
            let length = (valid_len - bytes.len()).min(self.maximum_read_bytes);
            let length =
                u32::try_from(length).context("attachment request length does not fit protocol")?;
            let request = ReadRequest {
                offset: request_offset,
                length,
            };
            write_request(&mut self.socket, request).context("write attachment range request")?;
            let response = read_response(&mut self.socket, length)
                .context("read attachment range response")?
                .ok_or_else(|| anyhow!("attachment range socket closed"))?;
            match response.status {
                ResponseStatus::Data => {
                    if response.payload.is_empty() {
                        bail!("attachment source ended before its advertised length");
                    }
                    bytes.extend_from_slice(&response.payload);
                    if response.payload.len() < length as usize {
                        if bytes.len() != valid_len {
                            bail!("attachment source returned a premature short read");
                        }
                        break;
                    }
                }
                ResponseStatus::InvalidRequest
                | ResponseStatus::SourceChanged
                | ResponseStatus::IoFailure => {
                    let diagnostic = String::from_utf8_lossy(&response.payload);
                    bail!("attachment source rejected range read: {diagnostic}");
                }
            }
        }
        if bytes.len() != valid_len {
            bail!("attachment source block length does not match advertised bytes");
        }
        if self.blocks.len() >= BLOCK_CACHE_ENTRIES {
            let evict = self
                .blocks
                .iter()
                .enumerate()
                .min_by_key(|(_, block)| block.touched)
                .map(|(index, _)| index)
                .expect("full attachment block cache is non-empty");
            self.blocks.swap_remove(evict);
        }
        self.blocks.push(CachedBlock {
            index,
            valid_len: bytes.len(),
            bytes,
            touched: self.clock,
        });
        Ok(self.blocks.last().expect("inserted attachment block"))
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredAttachmentSource {
    source: Arc<AttachmentSource>,
    url: Arc<str>,
}

impl RegisteredAttachmentSource {
    pub(crate) fn source(&self) -> &Arc<AttachmentSource> {
        &self.source
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

#[derive(Clone)]
pub(crate) struct AttachmentSourceRegistry {
    state: Arc<Mutex<RegistryState>>,
}

struct RegistryState {
    namespace: u64,
    next_token: u64,
    entries: HashMap<String, Weak<AttachmentSource>>,
    source_tokens: HashMap<AttachmentSourceKey, String>,
}

impl AttachmentSourceRegistry {
    pub(crate) fn new(namespace: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState {
                namespace,
                next_token: 1,
                entries: HashMap::new(),
                source_tokens: HashMap::new(),
            })),
        }
    }

    pub(crate) fn register(&self, source: Arc<AttachmentSource>) -> RegisteredAttachmentSource {
        let mut state = self.state.lock().expect("source registry lock poisoned");
        state.remove_dead();
        let token = state
            .source_tokens
            .get(&source.key())
            .filter(|token| state.entries.get(*token).and_then(Weak::upgrade).is_some())
            .cloned()
            .unwrap_or_else(|| {
                let token = format!("{:016x}{:016x}", state.namespace, state.next_token);
                state.next_token = state.next_token.wrapping_add(1).max(1);
                state.source_tokens.insert(source.key(), token.clone());
                state.entries.insert(token.clone(), Arc::downgrade(&source));
                token
            });
        RegisteredAttachmentSource {
            source,
            url: format!("{MPV_SCHEME}{token}").into(),
        }
    }

    pub(crate) fn clear(&self, namespace: u64) {
        let mut state = self.state.lock().expect("source registry lock poisoned");
        state.namespace = namespace;
        state.next_token = 1;
        state.entries.clear();
        state.source_tokens.clear();
    }

    fn resolve_url(&self, uri: &str) -> Option<Arc<AttachmentSource>> {
        let token = uri.strip_prefix(MPV_SCHEME)?;
        if token.is_empty() || token.contains('/') {
            return None;
        }
        let mut state = self.state.lock().ok()?;
        let source = state.entries.get(token).and_then(Weak::upgrade);
        if source.is_none() {
            state.entries.remove(token);
            state.source_tokens.retain(|_, value| value != token);
        }
        source
    }
}

impl RegistryState {
    fn remove_dead(&mut self) {
        self.entries.retain(|_, source| source.strong_count() > 0);
        self.source_tokens
            .retain(|_, token| self.entries.contains_key(token));
    }
}

const MAX_OPENING_SOURCES: usize = 4;
const MAX_READY_SOURCES: usize = 16;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_SHIFT: u32 = 5;

#[derive(Clone)]
pub(crate) enum VideoSourceView {
    Absent,
    Loading,
    Ready(RegisteredAttachmentSource),
    Failed { reason: Arc<str>, retryable: bool },
}

pub(crate) struct VideoSourceCandidate {
    pub key: AttachmentSourceKey,
    pub descriptor: AttachmentDescriptor,
    pub visible: bool,
}

pub(crate) struct SourceOpenRequest {
    pub key: AttachmentSourceKey,
    pub request_id: RequestId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoSourcePin {
    Playing,
    Theater,
    Thumbnail,
    PendingPlay,
}

impl VideoSourcePin {
    const fn bit(self) -> u8 {
        match self {
            Self::Playing => 1 << 0,
            Self::Theater => 1 << 1,
            Self::Thumbnail => 1 << 2,
            Self::PendingPlay => 1 << 3,
        }
    }
}

const PLAYBACK_PINS: u8 = VideoSourcePin::Playing.bit()
    | VideoSourcePin::Theater.bit()
    | VideoSourcePin::PendingPlay.bit();

pub(crate) struct VideoSourceCache {
    entries: HashMap<AttachmentSourceKey, VideoSourceEntry>,
    requests: HashMap<RequestId, AttachmentSourceKey>,
    namespace: u64,
    visibility_epoch: u64,
    clock: u64,
    maximum_remote_sources: usize,
}

struct VideoSourceEntry {
    descriptor: AttachmentDescriptor,
    state: VideoSourceEntryState,
    priority: Option<SourcePriority>,
    desired: bool,
    pins: u8,
    touched: u64,
    failures: u32,
    known_remote: Option<bool>,
}

enum VideoSourceEntryState {
    Absent,
    Queued,
    Opening(RequestId),
    Ready(RegisteredAttachmentSource),
    Failed {
        reason: Arc<str>,
        retry_at: Instant,
        retryable: bool,
    },
}

#[derive(Clone, Copy)]
struct SourcePriority {
    tier: u8,
    rank: usize,
    epoch: u64,
}

impl VideoSourceCache {
    pub(crate) fn new(namespace: u64, maximum_remote_sources: u16) -> Self {
        Self {
            entries: HashMap::new(),
            requests: HashMap::new(),
            namespace,
            visibility_epoch: 0,
            clock: 0,
            maximum_remote_sources: sanitize_stream_limit(maximum_remote_sources),
        }
    }

    pub(crate) fn update_limits(&mut self, maximum_remote_sources: u16) {
        self.maximum_remote_sources = sanitize_stream_limit(maximum_remote_sources);
        self.evict_over_limits();
    }

    pub(crate) fn update_visibility(
        &mut self,
        candidates: Vec<VideoSourceCandidate>,
    ) -> Vec<RequestId> {
        self.visibility_epoch = self.visibility_epoch.wrapping_add(1);
        let epoch = self.visibility_epoch;
        let desired = candidates
            .iter()
            .map(|candidate| candidate.key)
            .collect::<HashSet<_>>();
        let stale = self
            .entries
            .iter()
            .filter(|(key, entry)| !desired.contains(key) && entry.pins == 0)
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        let mut canceled = Vec::new();
        for key in stale {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            entry.desired = false;
            entry.priority = None;
            match entry.state {
                VideoSourceEntryState::Queued => {
                    entry.state = VideoSourceEntryState::Absent;
                }
                VideoSourceEntryState::Opening(request_id) => {
                    self.requests.remove(&request_id);
                    entry.state = VideoSourceEntryState::Absent;
                    canceled.push(request_id);
                }
                _ => {}
            }
        }

        for (rank, candidate) in candidates.into_iter().enumerate() {
            if candidate.key.namespace != self.namespace {
                continue;
            }
            let priority = SourcePriority {
                tier: if candidate.visible { 0 } else { 1 },
                rank,
                epoch,
            };
            let entry = self
                .entries
                .entry(candidate.key)
                .or_insert_with(|| VideoSourceEntry {
                    descriptor: candidate.descriptor.clone(),
                    state: VideoSourceEntryState::Absent,
                    priority: None,
                    desired: true,
                    pins: 0,
                    touched: 0,
                    failures: 0,
                    known_remote: None,
                });
            entry.descriptor = candidate.descriptor;
            entry.desired = true;
            entry.priority = Some(priority);
            if matches!(entry.state, VideoSourceEntryState::Absent) {
                entry.state = VideoSourceEntryState::Queued;
            }
        }
        canceled
    }

    pub(crate) fn promote(&mut self, key: AttachmentSourceKey, descriptor: AttachmentDescriptor) {
        if key.namespace != self.namespace {
            return;
        }
        self.visibility_epoch = self.visibility_epoch.wrapping_add(1);
        let entry = self.entries.entry(key).or_insert_with(|| VideoSourceEntry {
            descriptor: descriptor.clone(),
            state: VideoSourceEntryState::Absent,
            priority: None,
            desired: true,
            pins: 0,
            touched: 0,
            failures: 0,
            known_remote: None,
        });
        entry.descriptor = descriptor;
        entry.desired = true;
        entry.priority = Some(SourcePriority {
            tier: 0,
            rank: 0,
            epoch: self.visibility_epoch,
        });
        if matches!(entry.state, VideoSourceEntryState::Absent) {
            entry.state = VideoSourceEntryState::Queued;
        }
    }

    pub(crate) fn retry(&mut self, key: AttachmentSourceKey) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if matches!(entry.state, VideoSourceEntryState::Failed { .. }) {
            entry.state = VideoSourceEntryState::Queued;
            entry.desired = true;
            entry.priority.get_or_insert(SourcePriority {
                tier: 0,
                rank: 0,
                epoch: self.visibility_epoch,
            });
        }
    }

    pub(crate) fn set_pin(&mut self, key: AttachmentSourceKey, pin: VideoSourcePin, enabled: bool) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if enabled {
            entry.pins |= pin.bit();
        } else {
            entry.pins &= !pin.bit();
        }
        if enabled {
            entry.desired = true;
        }
        self.evict_over_limits();
    }

    /// Whether playback holds this source. Thumbnail extraction shares the
    /// source's socket, block cache, and lock with mpv, so it stays out of the
    /// way while playback needs them.
    pub(crate) fn has_playback_pin(&self, key: AttachmentSourceKey) -> bool {
        self.entries
            .get(&key)
            .is_some_and(|entry| entry.pins & PLAYBACK_PINS != 0)
    }

    pub(crate) fn thumbnail_finished(&mut self, key: AttachmentSourceKey) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        entry.pins &= !VideoSourcePin::Thumbnail.bit();
        if entry.pins == 0 {
            entry.desired = false;
            entry.priority = None;
        }
        self.evict_over_limits();
    }

    pub(crate) fn sync_pins(&mut self, pin: VideoSourcePin, pinned: &HashSet<AttachmentSourceKey>) {
        for (key, entry) in &mut self.entries {
            if pinned.contains(key) {
                entry.pins |= pin.bit();
                entry.desired = true;
            } else {
                entry.pins &= !pin.bit();
            }
        }
        self.evict_over_limits();
    }

    pub(crate) fn pending_descriptor(
        &self,
        request_id: RequestId,
    ) -> Option<&AttachmentDescriptor> {
        let key = self.requests.get(&request_id)?;
        self.entries.get(key).map(|entry| &entry.descriptor)
    }

    pub(crate) fn view(&mut self, key: AttachmentSourceKey) -> VideoSourceView {
        self.clock = self.clock.wrapping_add(1);
        let Some(entry) = self.entries.get_mut(&key) else {
            return VideoSourceView::Absent;
        };
        entry.touched = self.clock;
        match &entry.state {
            VideoSourceEntryState::Absent => VideoSourceView::Absent,
            VideoSourceEntryState::Queued | VideoSourceEntryState::Opening(_) => {
                VideoSourceView::Loading
            }
            VideoSourceEntryState::Ready(source) => VideoSourceView::Ready(source.clone()),
            VideoSourceEntryState::Failed {
                reason, retryable, ..
            } => VideoSourceView::Failed {
                reason: reason.clone(),
                retryable: *retryable,
            },
        }
    }

    pub(crate) fn pending_key(&self, request_id: RequestId) -> Option<AttachmentSourceKey> {
        self.requests.get(&request_id).copied()
    }

    pub(crate) fn start_next(
        &mut self,
        request_id: RequestId,
        now: Instant,
    ) -> Option<SourceOpenRequest> {
        self.activate_due_retries(now);
        if self.requests.len() >= MAX_OPENING_SOURCES {
            return None;
        }
        let key = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, VideoSourceEntryState::Queued))
            .filter_map(|(key, entry)| {
                entry.priority.map(|priority| {
                    (
                        *key,
                        priority.tier,
                        priority.rank,
                        Reverse(priority.epoch),
                        entry.touched,
                    )
                })
            })
            .min_by_key(|(_, tier, rank, epoch, touched)| (*tier, *rank, *epoch, *touched))
            .map(|(key, ..)| key)?;
        if !self.make_capacity_for(key) {
            return None;
        }
        let entry = self.entries.get_mut(&key)?;
        entry.state = VideoSourceEntryState::Opening(request_id);
        self.requests.insert(request_id, key);
        Some(SourceOpenRequest { key, request_id })
    }

    pub(crate) fn opened(
        &mut self,
        request_id: RequestId,
        source: Arc<AttachmentSource>,
        registry: &AttachmentSourceRegistry,
    ) -> Result<RegisteredAttachmentSource> {
        let key = self
            .requests
            .remove(&request_id)
            .ok_or_else(|| anyhow!("attachment source request is no longer pending"))?;
        if source.key() != key {
            bail!("attachment source response identity does not match request");
        }
        let entry = self
            .entries
            .get_mut(&key)
            .ok_or_else(|| anyhow!("attachment source request entry is missing"))?;
        if !matches!(entry.state, VideoSourceEntryState::Opening(id) if id == request_id) {
            bail!("attachment source request state does not match response");
        }
        let registered = registry.register(source);
        entry.known_remote = Some(registered.source().is_remote());
        entry.state = VideoSourceEntryState::Ready(registered.clone());
        entry.failures = 0;
        self.clock = self.clock.wrapping_add(1);
        entry.touched = self.clock;
        self.evict_over_limits();
        Ok(registered)
    }

    pub(crate) fn rejected(
        &mut self,
        request_id: RequestId,
        code: u16,
        reason: String,
        now: Instant,
    ) -> bool {
        let Some(key) = self.requests.remove(&request_id) else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        entry.failures = entry.failures.saturating_add(1);
        let retryable = code == 429;
        let retry_at = now + retry_delay(entry.failures);
        entry.state = VideoSourceEntryState::Failed {
            reason: reason.into(),
            retry_at,
            retryable,
        };
        true
    }

    pub(crate) fn failed_to_send(&mut self, request_id: RequestId, reason: String, now: Instant) {
        let _ = self.rejected(request_id, 503, reason, now);
    }

    pub(crate) fn source_failed(&mut self, key: AttachmentSourceKey, reason: String, now: Instant) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if !matches!(entry.state, VideoSourceEntryState::Ready(_)) {
            return;
        }
        entry.failures = entry.failures.saturating_add(1);
        entry.state = VideoSourceEntryState::Failed {
            reason: reason.into(),
            retry_at: now + retry_delay(entry.failures),
            retryable: true,
        };
    }

    pub(crate) fn next_retry_at(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter(|entry| entry.desired)
            .filter_map(|entry| match entry.state {
                VideoSourceEntryState::Failed {
                    retry_at,
                    retryable: true,
                    ..
                } => Some(retry_at),
                _ => None,
            })
            .min()
    }

    pub(crate) fn reset(&mut self, namespace: u64, maximum_remote_sources: u16) -> Vec<RequestId> {
        let canceled = self.requests.keys().copied().collect();
        self.entries.clear();
        self.requests.clear();
        self.namespace = namespace;
        self.visibility_epoch = 0;
        self.maximum_remote_sources = sanitize_stream_limit(maximum_remote_sources);
        canceled
    }

    fn activate_due_retries(&mut self, now: Instant) {
        for entry in self.entries.values_mut() {
            if entry.desired
                && matches!(
                    entry.state,
                    VideoSourceEntryState::Failed {
                        retry_at,
                        retryable: true,
                        ..
                    } if now >= retry_at
                )
            {
                entry.state = VideoSourceEntryState::Queued;
            }
        }
    }

    fn make_capacity_for(&mut self, key: AttachmentSourceKey) -> bool {
        let wants_remote = self
            .entries
            .get(&key)
            .and_then(|entry| entry.known_remote)
            .unwrap_or(true);
        if wants_remote && self.ready_remote_count() >= self.maximum_remote_sources {
            if !self.evict_one_ready(true) {
                return false;
            }
        }
        if self.ready_count() >= MAX_READY_SOURCES {
            if !self.evict_one_ready(false) {
                return false;
            }
        }
        true
    }

    fn evict_over_limits(&mut self) {
        while self.ready_remote_count() > self.maximum_remote_sources {
            if !self.evict_one_ready(true) {
                break;
            }
        }
        while self.ready_count() > MAX_READY_SOURCES {
            if !self.evict_one_ready(false) {
                break;
            }
        }
    }

    fn evict_one_ready(&mut self, remote_only: bool) -> bool {
        let key = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.pins == 0)
            .filter(|(_, entry)| {
                matches!(&entry.state, VideoSourceEntryState::Ready(source)
                    if !remote_only || source.source().is_remote())
            })
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| *key);
        let Some(key) = key else {
            return false;
        };
        let entry = self.entries.get_mut(&key).expect("selected source entry");
        entry.state = if entry.desired {
            VideoSourceEntryState::Queued
        } else {
            VideoSourceEntryState::Absent
        };
        true
    }

    fn ready_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry.state, VideoSourceEntryState::Ready(_)))
            .count()
    }

    fn ready_remote_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| {
                matches!(&entry.state, VideoSourceEntryState::Ready(source)
                    if source.source().is_remote())
            })
            .count()
    }
}

fn sanitize_stream_limit(limit: u16) -> usize {
    usize::from(limit).clamp(1, local_rpc::MAX_CONCURRENT_ATTACHMENT_STREAMS)
}

fn retry_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(MAX_RETRY_SHIFT);
    RETRY_BASE_DELAY.saturating_mul(1u32 << shift)
}

pub(crate) struct AttachmentCursor {
    source: Option<Arc<AttachmentSource>>,
    position: u64,
    opened_at: Instant,
    read_count: u64,
    bytes_read: u64,
    read_elapsed: Duration,
    seek_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum AttachmentSeekMode {
    Set,
    Current,
    End,
}

impl AttachmentCursor {
    pub(crate) fn new(source: Option<Arc<AttachmentSource>>) -> Self {
        Self {
            source,
            position: 0,
            opened_at: Instant::now(),
            read_count: 0,
            bytes_read: 0,
            read_elapsed: Duration::ZERO,
            seek_count: 0,
        }
    }

    pub(crate) fn read(&mut self, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("attachment source token is unknown or expired"))?;
        if self.position >= source.byte_len() {
            return Ok(0);
        }
        let offset = self.position;
        let started_at = Instant::now();
        let read = source.read_at(self.position, output)?;
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("attachment cursor position overflow"))?;
        let elapsed = started_at.elapsed();
        self.read_count = self.read_count.saturating_add(1);
        self.bytes_read = self.bytes_read.saturating_add(read as u64);
        self.read_elapsed = self.read_elapsed.saturating_add(elapsed);
        if self.read_count == 1 {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                let key = source.key();
                kvlog::info!(
                    "attachment protocol first read",
                    group = "media",
                    namespace = key.namespace,
                    room_id = key.room_id,
                    attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                    attachment_transfer_id = key.attachment_id.transfer_id,
                    backend = if source.is_remote() {
                        "remote"
                    } else {
                        "direct"
                    },
                    offset,
                    requested = output.len(),
                    read,
                    elapsed_ms = elapsed.as_secs_f64() * 1_000.0,
                    cumulative_read_ms = self.read_elapsed.as_secs_f64() * 1_000.0
                );
            }
        } else if elapsed >= Duration::from_millis(10) {
            let key = source.key();
            kvlog::warn!(
                "slow attachment protocol read",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                backend = if source.is_remote() {
                    "remote"
                } else {
                    "direct"
                },
                offset,
                requested = output.len(),
                read,
                elapsed_ms = elapsed.as_secs_f64() * 1_000.0,
                ordinal = self.read_count,
                cumulative_read_ms = self.read_elapsed.as_secs_f64() * 1_000.0
            );
        } else if self.read_count <= STARTUP_READ_TRACE_LIMIT {
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::media_logging_enabled() {
                let key = source.key();
                kvlog::info!(
                    "attachment protocol startup read",
                    group = "media",
                    namespace = key.namespace,
                    room_id = key.room_id,
                    attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                    attachment_transfer_id = key.attachment_id.transfer_id,
                    backend = if source.is_remote() {
                        "remote"
                    } else {
                        "direct"
                    },
                    ordinal = self.read_count,
                    offset,
                    requested = output.len(),
                    read,
                    elapsed_ms = elapsed.as_secs_f64() * 1_000.0,
                    cumulative_read_ms = self.read_elapsed.as_secs_f64() * 1_000.0
                );
            }
        }
        Ok(read)
    }

    pub(crate) fn seek(&mut self, offset: i64, mode: AttachmentSeekMode) -> Result<u64> {
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("attachment source token is unknown or expired"))?;
        let base = match mode {
            AttachmentSeekMode::Set => 0i128,
            AttachmentSeekMode::Current => i128::from(self.position),
            AttachmentSeekMode::End => i128::from(source.byte_len()),
        };
        let _previous = self.position;
        let position = (base + i128::from(offset)).clamp(0, i128::from(source.byte_len()));
        self.position = position as u64;
        self.seek_count = self.seek_count.saturating_add(1);
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let key = source.key();
            let mode = match mode {
                AttachmentSeekMode::Set => "set",
                AttachmentSeekMode::Current => "current",
                AttachmentSeekMode::End => "end",
            };
            kvlog::info!(
                "attachment protocol seek",
                group = "media",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                mode,
                offset,
                previous = _previous,
                position = self.position
            );
        }
        Ok(self.position)
    }

    pub(crate) fn size(&self) -> Result<i64> {
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| anyhow!("attachment source token is unknown or expired"))?;
        i64::try_from(source.byte_len()).context("attachment source length exceeds libmpv")
    }
}

impl Drop for AttachmentCursor {
    fn drop(&mut self) {
        let Some(_source) = self.source.as_ref() else {
            kvlog::warn!(
                "unresolved attachment protocol cursor closed",
                elapsed_ms = self.opened_at.elapsed().as_secs_f64() * 1_000.0
            );
            return;
        };
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let key = _source.key();
            kvlog::info!(
                "attachment protocol cursor closed",
                group = "media",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                backend = if _source.is_remote() {
                    "remote"
                } else {
                    "direct"
                },
                reads = self.read_count,
                size = self.bytes_read,
                seeks = self.seek_count,
                final_offset = self.position,
                cumulative_read_ms = self.read_elapsed.as_secs_f64() * 1_000.0,
                elapsed_ms = self.opened_at.elapsed().as_secs_f64() * 1_000.0
            );
        }
    }
}

pub(crate) fn register_mpv_attachment_protocol(
    mpv: &Mpv,
    registry: AttachmentSourceRegistry,
) -> Result<()> {
    unsafe {
        libmpv2::protocol::register_owned(
            mpv,
            MPV_PROTOCOL,
            registry,
            protocol_open,
            protocol_close,
            protocol_read,
            Some(protocol_seek),
            Some(protocol_size),
        )
    }
    .context("register daemon attachment protocol")
}

fn protocol_open(registry: &mut AttachmentSourceRegistry, uri: &str) -> AttachmentCursor {
    let started_at = Instant::now();
    let source = registry.resolve_url(uri);
    if let Some(_source) = source.as_ref() {
        #[cfg(feature = "diagnostic-logs")]
        if crate::logger::media_logging_enabled() {
            let key = _source.key();
            kvlog::info!(
                "attachment protocol opened",
                group = "media",
                namespace = key.namespace,
                room_id = key.room_id,
                attachment_timestamp_ms = key.attachment_id.timestamp_ms,
                attachment_transfer_id = key.attachment_id.transfer_id,
                backend = if _source.is_remote() {
                    "remote"
                } else {
                    "direct"
                },
                size = _source.byte_len(),
                elapsed_ms = started_at.elapsed().as_secs_f64() * 1_000.0
            );
        }
    } else {
        kvlog::warn!(
            "attachment protocol could not resolve source token",
            elapsed_ms = started_at.elapsed().as_secs_f64() * 1_000.0
        );
    }
    AttachmentCursor::new(source)
}

fn protocol_close(cursor: Box<AttachmentCursor>) {
    drop(cursor);
}

fn protocol_read(cursor: &mut AttachmentCursor, output: &mut [c_char]) -> i64 {
    if output.is_empty() {
        return 0;
    }
    let output =
        unsafe { std::slice::from_raw_parts_mut(output.as_mut_ptr().cast::<u8>(), output.len()) };
    match cursor.read(output) {
        Ok(read) => i64::try_from(read).unwrap_or(-1),
        Err(error) => {
            kvlog::warn!("libmpv attachment read failed", err = %error);
            -1
        }
    }
}

fn protocol_seek(cursor: &mut AttachmentCursor, offset: i64) -> i64 {
    match cursor.seek(offset, AttachmentSeekMode::Set) {
        Ok(position) => i64::try_from(position).unwrap_or(-1),
        Err(error) => {
            kvlog::warn!("libmpv attachment seek failed", err = %error);
            -1
        }
    }
}

fn protocol_size(cursor: &mut AttachmentCursor) -> i64 {
    cursor.size().unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::attachment_stream::{read_request, write_response};
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    fn key(value: u64) -> AttachmentSourceKey {
        AttachmentSourceKey {
            namespace: 7,
            room_id: RoomId(8),
            attachment_id: AttachmentId {
                timestamp_ms: 9,
                transfer_id: local_rpc::ids::FileTransferId(value),
            },
        }
    }

    fn descriptor(value: u64) -> AttachmentDescriptor {
        AttachmentDescriptor {
            id: key(value).attachment_id,
            file_name: format!("video-{value}.mp4"),
            media_kind: local_rpc::model::MediaKind::Video,
            content_type: "video/mp4".into(),
            byte_len: 1,
            width: Some(16),
            height: Some(9),
        }
    }

    fn direct_source(key: AttachmentSourceKey) -> Arc<AttachmentSource> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"x").unwrap();
        AttachmentSource::direct(key, file, 1)
    }

    fn remote_source(
        bytes: Vec<u8>,
    ) -> (
        Arc<AttachmentSource>,
        Arc<AtomicUsize>,
        thread::JoinHandle<()>,
    ) {
        let byte_len = bytes.len() as u64;
        let (frontend, mut daemon) = UnixStream::pair().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let worker_requests = requests.clone();
        let worker = thread::spawn(move || {
            while let Some(request) = read_request(&mut daemon).unwrap() {
                worker_requests.fetch_add(1, Ordering::Relaxed);
                let start = request.offset as usize;
                let end = start
                    .saturating_add(request.length as usize)
                    .min(bytes.len());
                let payload = if start >= bytes.len() {
                    &[][..]
                } else {
                    &bytes[start..end]
                };
                write_response(&mut daemon, ResponseStatus::Data, payload).unwrap();
            }
        });
        (
            AttachmentSource::remote(
                key(10),
                frontend,
                byte_len,
                local_rpc::MAX_ATTACHMENT_READ_BYTES,
            ),
            requests,
            worker,
        )
    }

    #[test]
    fn remote_reads_cache_aligned_blocks_and_retain_short_final_length() {
        let bytes = (0..(BLOCK_BYTES + 37))
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let expected = bytes.clone();
        let (source, requests, worker) = remote_source(bytes);

        let mut first = [0; 32];
        source.read_at(3, &mut first).unwrap();
        assert_eq!(first, expected[3..35]);
        let mut cached = [0; 16];
        source.read_at(100, &mut cached).unwrap();
        assert_eq!(cached, expected[100..116]);
        assert_eq!(requests.load(Ordering::Relaxed), 1);

        let mut tail = [0; 100];
        assert_eq!(source.read_at(BLOCK_BYTES as u64, &mut tail).unwrap(), 37);
        assert_eq!(&tail[..37], &expected[BLOCK_BYTES..]);
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        drop(source);
        worker.join().unwrap();
    }

    #[test]
    fn zero_length_and_eof_reads_do_not_use_the_socket() {
        let (source, requests, worker) = remote_source(vec![1, 2, 3]);
        assert_eq!(source.read_at(0, &mut []).unwrap(), 0);
        assert_eq!(source.read_at(3, &mut [0; 1]).unwrap(), 0);
        assert_eq!(source.read_at(30, &mut [0; 1]).unwrap(), 0);
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        drop(source);
        worker.join().unwrap();
    }

    #[test]
    fn reports_premature_remote_eof() {
        let (frontend, mut daemon) = UnixStream::pair().unwrap();
        let source =
            AttachmentSource::remote(key(11), frontend, 32, local_rpc::MAX_ATTACHMENT_READ_BYTES);
        let worker = thread::spawn(move || {
            let request = read_request(&mut daemon).unwrap().unwrap();
            assert_eq!(request.offset, 0);
            write_response(&mut daemon, ResponseStatus::Data, b"short").unwrap();
        });
        let error = source.read_at(0, &mut [0; 16]).unwrap_err();
        assert!(format!("{error:#}").contains("premature short read"));
        worker.join().unwrap();
    }

    #[test]
    fn independent_cursors_share_source_and_seek_modes_clamp() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"0123456789").unwrap();
        let source = AttachmentSource::direct(key(12), file, 10);
        let mut first = AttachmentCursor::new(Some(source.clone()));
        let mut second = AttachmentCursor::new(Some(source));

        first.seek(4, AttachmentSeekMode::Set).unwrap();
        second.seek(-2, AttachmentSeekMode::End).unwrap();
        let mut bytes = [0; 2];
        first.read(&mut bytes).unwrap();
        assert_eq!(&bytes, b"45");
        second.read(&mut bytes).unwrap();
        assert_eq!(&bytes, b"89");
        assert_eq!(first.read_count, 1);
        assert_eq!(first.bytes_read, 2);
        assert!(first.read_elapsed <= first.opened_at.elapsed());

        assert_eq!(first.seek(-100, AttachmentSeekMode::Current).unwrap(), 0);
        assert_eq!(first.seek(100, AttachmentSeekMode::Current).unwrap(), 10);
        assert_eq!(first.read(&mut bytes).unwrap(), 0);
    }

    #[test]
    fn registry_tokens_expire_on_namespace_clear() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"abc").unwrap();
        let registry = AttachmentSourceRegistry::new(7);
        let registered = registry.register(AttachmentSource::direct(key(13), file, 3));
        assert!(registry.resolve_url(registered.url()).is_some());
        registry.clear(8);
        assert!(registry.resolve_url(registered.url()).is_none());
    }

    #[test]
    fn source_requests_deduplicate_and_obey_four_opening_slots() {
        let mut cache = VideoSourceCache::new(7, 8);
        let candidates = (1..=8)
            .flat_map(|value| {
                let candidate = VideoSourceCandidate {
                    key: key(value),
                    descriptor: descriptor(value),
                    visible: true,
                };
                [
                    candidate,
                    VideoSourceCandidate {
                        key: key(value),
                        descriptor: descriptor(value),
                        visible: true,
                    },
                ]
            })
            .collect();
        cache.update_visibility(candidates);
        assert_eq!(cache.entries.len(), 8);
        for request_id in 1..=4 {
            assert!(
                cache
                    .start_next(RequestId(request_id), Instant::now())
                    .is_some()
            );
        }
        assert!(cache.start_next(RequestId(5), Instant::now()).is_none());
        assert_eq!(cache.requests.len(), MAX_OPENING_SOURCES);
    }

    #[test]
    fn visible_sources_outrank_overscan_and_fast_scroll_cancels_stale_opening() {
        let mut cache = VideoSourceCache::new(7, 8);
        cache.update_visibility(vec![
            VideoSourceCandidate {
                key: key(1),
                descriptor: descriptor(1),
                visible: false,
            },
            VideoSourceCandidate {
                key: key(2),
                descriptor: descriptor(2),
                visible: true,
            },
        ]);
        let first = cache.start_next(RequestId(10), Instant::now()).unwrap();
        assert_eq!(first.key, key(2));

        let canceled = cache.update_visibility(vec![VideoSourceCandidate {
            key: key(3),
            descriptor: descriptor(3),
            visible: true,
        }]);
        assert_eq!(canceled, vec![RequestId(10)]);
        assert!(cache.pending_key(RequestId(10)).is_none());
        assert_eq!(
            cache.start_next(RequestId(11), Instant::now()).unwrap().key,
            key(3)
        );
    }

    #[test]
    fn source_lru_does_not_evict_pinned_ready_entries() {
        let registry = AttachmentSourceRegistry::new(7);
        let mut cache = VideoSourceCache::new(7, 8);
        for value in 1..=MAX_READY_SOURCES as u64 {
            cache.promote(key(value), descriptor(value));
            let request_id = RequestId(value);
            cache.start_next(request_id, Instant::now()).unwrap();
            cache
                .opened(request_id, direct_source(key(value)), &registry)
                .unwrap();
        }
        cache.set_pin(key(1), VideoSourcePin::Playing, true);
        cache.promote(key(100), descriptor(100));
        cache.start_next(RequestId(100), Instant::now()).unwrap();
        cache
            .opened(RequestId(100), direct_source(key(100)), &registry)
            .unwrap();

        assert!(matches!(cache.view(key(1)), VideoSourceView::Ready(_)));
        assert!(matches!(cache.view(key(100)), VideoSourceView::Ready(_)));
        assert_eq!(cache.ready_count(), MAX_READY_SOURCES);
    }

    #[test]
    fn remote_source_slots_cycle_without_exceeding_negotiated_limit() {
        let registry = AttachmentSourceRegistry::new(7);
        let mut cache = VideoSourceCache::new(7, 2);
        let mut daemon_peers = Vec::new();
        for value in 1..=2 {
            cache.promote(key(value), descriptor(value));
            let request_id = RequestId(value);
            cache.start_next(request_id, Instant::now()).unwrap();
            let (frontend, daemon) = UnixStream::pair().unwrap();
            daemon_peers.push(daemon);
            cache
                .opened(
                    request_id,
                    AttachmentSource::remote(
                        key(value),
                        frontend,
                        1,
                        local_rpc::MAX_ATTACHMENT_READ_BYTES,
                    ),
                    &registry,
                )
                .unwrap();
        }
        assert_eq!(cache.ready_remote_count(), 2);

        cache.promote(key(3), descriptor(3));
        cache.start_next(RequestId(3), Instant::now()).unwrap();
        assert_eq!(cache.ready_remote_count(), 1);
        let (frontend, daemon) = UnixStream::pair().unwrap();
        daemon_peers.push(daemon);
        cache
            .opened(
                RequestId(3),
                AttachmentSource::remote(key(3), frontend, 1, local_rpc::MAX_ATTACHMENT_READ_BYTES),
                &registry,
            )
            .unwrap();
        assert_eq!(cache.ready_remote_count(), 2);
    }

    #[test]
    fn completed_thumbnail_source_does_not_requeue_after_lru_eviction() {
        let registry = AttachmentSourceRegistry::new(7);
        let mut cache = VideoSourceCache::new(7, 1);
        cache.promote(key(1), descriptor(1));
        cache.start_next(RequestId(1), Instant::now()).unwrap();
        let (first_frontend, first_daemon) = UnixStream::pair().unwrap();
        cache
            .opened(
                RequestId(1),
                AttachmentSource::remote(
                    key(1),
                    first_frontend,
                    1,
                    local_rpc::MAX_ATTACHMENT_READ_BYTES,
                ),
                &registry,
            )
            .unwrap();
        cache.set_pin(key(1), VideoSourcePin::Thumbnail, true);
        cache.thumbnail_finished(key(1));

        cache.promote(key(2), descriptor(2));
        cache.start_next(RequestId(2), Instant::now()).unwrap();
        assert!(matches!(cache.view(key(1)), VideoSourceView::Absent));
        let (second_frontend, second_daemon) = UnixStream::pair().unwrap();
        cache
            .opened(
                RequestId(2),
                AttachmentSource::remote(
                    key(2),
                    second_frontend,
                    1,
                    local_rpc::MAX_ATTACHMENT_READ_BYTES,
                ),
                &registry,
            )
            .unwrap();
        assert!(cache.start_next(RequestId(3), Instant::now()).is_none());
        drop((first_daemon, second_daemon));
    }

    #[test]
    fn stream_cap_rejections_retry_automatically_but_permanent_failures_do_not() {
        let mut cache = VideoSourceCache::new(7, 8);
        cache.promote(key(1), descriptor(1));
        let now = Instant::now();
        cache.start_next(RequestId(1), now).unwrap();
        assert!(cache.rejected(RequestId(1), 429, "stream cap".into(), now));
        assert!(cache.next_retry_at().is_some());
        assert!(cache.start_next(RequestId(2), now).is_none());
        assert!(
            cache
                .start_next(RequestId(2), now + Duration::from_secs(65))
                .is_some()
        );

        cache.promote(key(3), descriptor(3));
        cache.start_next(RequestId(3), now).unwrap();
        assert!(cache.rejected(RequestId(3), 404, "missing".into(), now));
        assert!(matches!(
            cache.view(key(3)),
            VideoSourceView::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn socket_shutdown_surfaces_as_source_failure() {
        let (frontend, daemon) = UnixStream::pair().unwrap();
        let source =
            AttachmentSource::remote(key(20), frontend, 8, local_rpc::MAX_ATTACHMENT_READ_BYTES);
        drop(daemon);
        let error = source.read_at(0, &mut [0; 1]).unwrap_err();
        assert!(!format!("{error:#}").is_empty());
        assert!(source.has_failed());
    }
}
