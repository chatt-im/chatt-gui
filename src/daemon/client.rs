use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Read,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::Duration,
};

use async_channel::{Receiver as EventReceiver, Sender as EventSender};
use local_rpc::{
    bulk::{BeginUpload, BulkFinished},
    frame::{ClientFrame, ClientHello, DaemonFrame, Operation, RequestOutcome},
    model::{AttachmentDescriptor, BulkTransferId, RequestId},
    unix::{ConnectError, FrameReader, FrameWriter},
};

#[derive(Debug)]
pub enum DaemonEvent {
    Discovering,
    Connecting,
    TransportConnected,
    Frame(DaemonFrame),
    LiveShareOpened {
        request_id: RequestId,
        stream_id: local_rpc::ids::StreamId,
        generation: u64,
        status: local_rpc::model::LiveShareViewStatus,
        stream: std::os::unix::net::UnixStream,
    },
    AttachmentSourceOpened {
        request_id: RequestId,
        room_id: local_rpc::ids::RoomId,
        attachment_id: local_rpc::model::AttachmentId,
        byte_len: u64,
        transport: local_rpc::frame::AttachmentSourceTransport,
        fd: OwnedFd,
    },
    Disconnected(String),
    Incompatible(String),
    UploadFailed {
        begin_request: RequestId,
        finish_request: RequestId,
        reason: String,
    },
    MediaCached(AttachmentDescriptor),
    MediaTransferFailed {
        transfer_id: BulkTransferId,
        reason: String,
    },
}

#[derive(Clone)]
pub struct DaemonClient {
    commands: SyncSender<ConnectorCommand>,
    events: EventSender<DaemonEvent>,
}

enum ConnectorCommand {
    Frame(ClientFrame),
    PreparedUpload(PreparedUpload),
    BeginUploadResult(local_rpc::frame::RequestResult),
    ChunkBytes(usize),
    CancelBulk(BulkTransferId),
    SessionEnded,
    Retry,
}

struct PreparedUpload {
    begin_request: RequestId,
    finish_request: RequestId,
    upload: BeginUpload,
    source: UploadSource,
}

enum UploadSource {
    File(File),
    Memory { bytes: Arc<Vec<u8>>, offset: usize },
}

impl Read for UploadSource {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.read(buffer),
            Self::Memory { bytes, offset } => {
                let remaining = &bytes[*offset..];
                let read = remaining.len().min(buffer.len());
                buffer[..read].copy_from_slice(&remaining[..read]);
                *offset += read;
                Ok(read)
            }
        }
    }
}

struct ActiveUpload {
    prepared: PreparedUpload,
    buffer: Vec<u8>,
    sent: u64,
}

impl DaemonClient {
    pub fn spawn(
        media_cache: std::sync::Arc<std::sync::Mutex<crate::media_cache::MediaCache>>,
    ) -> (Self, EventReceiver<DaemonEvent>) {
        let (command_tx, command_rx) = mpsc::sync_channel(128);
        let (event_tx, event_rx) = async_channel::bounded(64);
        let connector_events = event_tx.clone();
        let connector_commands = command_tx.clone();
        thread::Builder::new()
            .name("chatt-gui-daemon".into())
            .spawn(move || {
                connection_loop(
                    command_rx,
                    connector_commands,
                    connector_events,
                    media_cache,
                )
            })
            .expect("failed to spawn daemon connector");
        (
            Self {
                commands: command_tx,
                events: event_tx,
            },
            event_rx,
        )
    }

    pub fn send(&self, frame: ClientFrame) -> Result<(), String> {
        let request_id = frame.request_id();
        self.commands
            .try_send(ConnectorCommand::Frame(frame))
            .map_err(|error| {
                let reason: String = match error {
                    TrySendError::Full(_) => "daemon command queue is full".into(),
                    TrySendError::Disconnected(_) => "daemon connector stopped".into(),
                };
                kvlog::error!(
                    "could not enqueue daemon request",
                    request_id,
                    err = %reason
                );
                reason
            })
    }

    pub fn upload_file(
        &self,
        path: PathBuf,
        room_id: local_rpc::ids::RoomId,
        transfer_id: BulkTransferId,
        begin_request: RequestId,
        finish_request: RequestId,
        max_upload_bytes: u64,
    ) {
        let commands = self.commands.clone();
        let events = self.events.clone();
        let spawn_errors = self.events.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("chatt-gui-upload-{}", transfer_id.0))
            .spawn(move || {
                let file = match File::open(&path) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = events.send_blocking(DaemonEvent::UploadFailed {
                            begin_request,
                            finish_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                let metadata = match file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = events.send_blocking(DaemonEvent::UploadFailed {
                            begin_request,
                            finish_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                if metadata.len() > max_upload_bytes {
                    let _ = events.send_blocking(DaemonEvent::UploadFailed {
                        begin_request,
                        finish_request,
                        reason: format!(
                            "upload is {} bytes; daemon limit is {max_upload_bytes} bytes",
                            metadata.len()
                        ),
                    });
                    return;
                }
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "upload".into());
                let upload = BeginUpload {
                    transfer_id,
                    room_id,
                    file_name,
                    byte_len: metadata.len(),
                };
                if commands
                    .send(ConnectorCommand::PreparedUpload(PreparedUpload {
                        begin_request,
                        finish_request,
                        upload,
                        source: UploadSource::File(file),
                    }))
                    .is_err()
                {
                    let _ = events.send_blocking(DaemonEvent::UploadFailed {
                        begin_request,
                        finish_request,
                        reason: "daemon connector stopped".into(),
                    });
                }
            })
        {
            let _ = spawn_errors.send(DaemonEvent::UploadFailed {
                begin_request,
                finish_request,
                reason: error.to_string(),
            });
        }
    }

    pub fn upload_bytes(
        &self,
        bytes: Arc<Vec<u8>>,
        file_name: String,
        room_id: local_rpc::ids::RoomId,
        transfer_id: BulkTransferId,
        begin_request: RequestId,
        finish_request: RequestId,
        max_upload_bytes: u64,
    ) -> Result<(), String> {
        let byte_len = bytes.len() as u64;
        if byte_len > max_upload_bytes {
            return Err(format!(
                "upload is {byte_len} bytes; daemon limit is {max_upload_bytes} bytes"
            ));
        }
        let upload = BeginUpload {
            transfer_id,
            room_id,
            file_name,
            byte_len,
        };
        self.commands
            .try_send(ConnectorCommand::PreparedUpload(PreparedUpload {
                begin_request,
                finish_request,
                upload,
                source: UploadSource::Memory { bytes, offset: 0 },
            }))
            .map_err(|error| match error {
                TrySendError::Full(_) => "daemon command queue is full".into(),
                TrySendError::Disconnected(_) => "daemon connector stopped".into(),
            })
    }

    pub fn retry(&self) {
        let _ = self.commands.try_send(ConnectorCommand::Retry);
    }

    pub fn disconnect_protocol(&self, reason: impl Into<String>) {
        let reason = reason.into();
        kvlog::error!("disconnecting daemon RPC after protocol error", err = %reason);
        let _ = self.commands.try_send(ConnectorCommand::SessionEnded);
    }
}

fn connection_loop(
    mut commands: Receiver<ConnectorCommand>,
    command_tx: SyncSender<ConnectorCommand>,
    events: EventSender<DaemonEvent>,
    media_cache: std::sync::Arc<std::sync::Mutex<crate::media_cache::MediaCache>>,
) {
    let mut delay = Duration::from_millis(250);
    loop {
        drain_stale_commands(&commands);
        let _ = events.try_send(DaemonEvent::Discovering);
        let hello = ClientHello::current(env!("CARGO_PKG_VERSION"));
        let _ = events.try_send(DaemonEvent::Connecting);
        match local_rpc::unix::connect(&hello) {
            Ok(stream) => {
                delay = Duration::from_millis(250);
                let _ = events.try_send(DaemonEvent::TransportConnected);
                let reader_stream = match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        kvlog::error!("could not clone daemon RPC stream", err = %error);
                        if events
                            .send_blocking(DaemonEvent::Disconnected(error.to_string()))
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                };
                let mut reader = FrameReader::new(reader_stream);
                let writer_events = events.clone();
                let writer_thread = match thread::Builder::new()
                    .name("chatt-gui-daemon-write".into())
                    .spawn(move || writer_loop(commands, stream, writer_events))
                {
                    Ok(thread) => thread,
                    Err(error) => {
                        kvlog::error!("could not start daemon RPC writer", err = %error);
                        let _ = events.send_blocking(DaemonEvent::Disconnected(error.to_string()));
                        return;
                    }
                };
                let reason = 'connected: loop {
                    match reader.recv_daemon_with_fds_and_bulk(|transfer_id, bytes| {
                        handle_bulk_chunk(transfer_id, bytes, &media_cache, &command_tx, &events);
                        Ok(())
                    }) {
                        Ok(None) => {}
                        Ok(Some(received)) => {
                            let frame = received.frame;
                            let mut fds = received.fds;
                            if let DaemonFrame::LiveShareOpened {
                                request_id,
                                stream_id,
                                generation,
                                status,
                            } = &frame
                            {
                                if fds.len() != 1 {
                                    break 'connected format!(
                                        "live share open carried {} descriptors instead of one",
                                        fds.len()
                                    );
                                }
                                let stream =
                                    std::os::unix::net::UnixStream::from(fds.pop().unwrap());
                                if events
                                    .send_blocking(DaemonEvent::LiveShareOpened {
                                        request_id: *request_id,
                                        stream_id: *stream_id,
                                        generation: *generation,
                                        status: *status,
                                        stream,
                                    })
                                    .is_err()
                                {
                                    let _ = command_tx.send(ConnectorCommand::SessionEnded);
                                    let _ = writer_thread.join();
                                    return;
                                }
                                continue;
                            }
                            if let DaemonFrame::AttachmentSourceOpened {
                                request_id,
                                room_id,
                                attachment_id,
                                byte_len,
                                transport,
                            } = &frame
                            {
                                if fds.len() != 1 {
                                    break 'connected format!(
                                        "attachment source open carried {} descriptors instead of one",
                                        fds.len()
                                    );
                                }
                                let fd = fds.pop().expect("validated attachment source fd");
                                if events
                                    .send_blocking(DaemonEvent::AttachmentSourceOpened {
                                        request_id: *request_id,
                                        room_id: *room_id,
                                        attachment_id: *attachment_id,
                                        byte_len: *byte_len,
                                        transport: *transport,
                                        fd,
                                    })
                                    .is_err()
                                {
                                    let _ = command_tx.send(ConnectorCommand::SessionEnded);
                                    let _ = writer_thread.join();
                                    return;
                                }
                                continue;
                            }
                            if !fds.is_empty() {
                                break 'connected "unexpected descriptors in daemon frame".into();
                            }
                            let frame = match handle_bulk_frame(
                                frame,
                                &media_cache,
                                &command_tx,
                                &events,
                            ) {
                                Some(frame) => frame,
                                None => continue,
                            };
                            if let DaemonFrame::Welcome(welcome) = &frame {
                                let chunk_bytes = usize::try_from(welcome.limits.chunk_bytes)
                                    .unwrap_or(local_rpc::MAX_CHUNK_BYTES)
                                    .min(local_rpc::MAX_CHUNK_BYTES)
                                    .max(1);
                                if command_tx
                                    .send(ConnectorCommand::ChunkBytes(chunk_bytes))
                                    .is_err()
                                {
                                    break 'connected "daemon writer stopped".into();
                                }
                            }
                            if let DaemonFrame::RequestResult(result) = &frame {
                                if result.operation == Operation::OpenAttachmentSource
                                    && matches!(result.outcome, RequestOutcome::Accepted)
                                {
                                    break 'connected
                                        "attachment source open completed with an accepted result instead of a descriptor"
                                            .into();
                                }
                                match &result.outcome {
                                    RequestOutcome::Accepted =>
                                    {
                                        #[cfg(feature = "diagnostic-logs")]
                                        if crate::logger::rpc_logging_enabled() {
                                            kvlog::info!(
                                                "daemon request accepted",
                                                group = "daemon-rpc",
                                                request_id = result.request_id,
                                                operation = result.operation
                                            );
                                        }
                                    }
                                    RequestOutcome::Rejected { code, message } => kvlog::error!(
                                        "daemon request rejected",
                                        request_id = result.request_id,
                                        operation = result.operation,
                                        code,
                                        err = %message
                                    ),
                                }
                            }
                            if let DaemonFrame::RequestResult(result) = &frame
                                && result.operation == Operation::BeginUpload
                            {
                                if command_tx
                                    .send(ConnectorCommand::BeginUploadResult(result.clone()))
                                    .is_err()
                                {
                                    break 'connected "daemon writer stopped".into();
                                }
                                continue;
                            }
                            if matches!(
                                &frame,
                                DaemonFrame::RequestResult(result)
                                    if result.request_id.0 & (1 << 63) != 0
                            ) {
                                continue;
                            }
                            if events.send_blocking(DaemonEvent::Frame(frame)).is_err() {
                                let _ = command_tx.send(ConnectorCommand::SessionEnded);
                                let _ = writer_thread.join();
                                return;
                            }
                        }
                        Err(error) => break error.to_string(),
                    }
                };
                let _ = command_tx.send(ConnectorCommand::SessionEnded);
                commands = match writer_thread.join() {
                    Ok(commands) => commands,
                    Err(_) => return,
                };
                media_cache
                    .lock()
                    .expect("media cache lock poisoned")
                    .cancel_all();
                kvlog::error!("daemon RPC connection ended", err = %reason);
                if events
                    .send_blocking(DaemonEvent::Disconnected(reason))
                    .is_err()
                {
                    return;
                }
            }
            Err(
                ConnectError::Incompatible(details)
                | ConnectError::Permission(details)
                | ConnectError::Rejected(details),
            ) => {
                kvlog::error!(
                    "daemon RPC connection rejected",
                    client_protocol_min = hello.min_version,
                    client_protocol_max = hello.max_version,
                    err = %details
                );
                if events
                    .send_blocking(DaemonEvent::Incompatible(details))
                    .is_err()
                {
                    return;
                }
                wait_for_retry(&commands, Duration::from_secs(5));
            }
            Err(error) => {
                kvlog::error!("daemon RPC connection failed", err = %error);
                if events
                    .send_blocking(DaemonEvent::Disconnected(error.to_string()))
                    .is_err()
                {
                    return;
                }
                wait_for_retry(&commands, delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

fn handle_bulk_frame(
    frame: DaemonFrame,
    media_cache: &std::sync::Mutex<crate::media_cache::MediaCache>,
    commands: &SyncSender<ConnectorCommand>,
    events: &EventSender<DaemonEvent>,
) -> Option<DaemonFrame> {
    match frame {
        DaemonFrame::BulkChunk(chunk) => {
            handle_bulk_chunk(
                chunk.transfer_id,
                &chunk.bytes,
                media_cache,
                commands,
                events,
            );
            None
        }
        DaemonFrame::BulkFinished(finished) => {
            let transfer_id = finished.transfer_id;
            #[cfg(feature = "diagnostic-logs")]
            if crate::logger::rpc_logging_enabled() {
                kvlog::info!(
                    "attachment transfer finished",
                    group = "daemon-rpc",
                    transfer_id
                );
            }
            match media_cache
                .lock()
                .expect("media cache lock poisoned")
                .finish(finished)
            {
                Ok(descriptor) => {
                    if events
                        .send_blocking(DaemonEvent::MediaCached(descriptor))
                        .is_err()
                    {
                        kvlog::error!("could not deliver attachment-cached event", transfer_id);
                    }
                }
                Err(reason) => cancel_failed_download(transfer_id, reason, commands, events),
            }
            None
        }
        DaemonFrame::BulkCanceled {
            transfer_id,
            reason,
        } => {
            kvlog::error!(
                "attachment transfer canceled",
                transfer_id,
                err = %reason
            );
            media_cache
                .lock()
                .expect("media cache lock poisoned")
                .cancel(transfer_id);
            let _ = events.send_blocking(DaemonEvent::MediaTransferFailed {
                transfer_id,
                reason,
            });
            None
        }
        frame => Some(frame),
    }
}

fn handle_bulk_chunk(
    transfer_id: BulkTransferId,
    bytes: &[u8],
    media_cache: &std::sync::Mutex<crate::media_cache::MediaCache>,
    commands: &SyncSender<ConnectorCommand>,
    events: &EventSender<DaemonEvent>,
) {
    if let Err(reason) = media_cache
        .lock()
        .expect("media cache lock poisoned")
        .chunk(transfer_id, bytes)
    {
        cancel_failed_download(transfer_id, reason, commands, events);
    }
}

fn cancel_failed_download(
    transfer_id: BulkTransferId,
    reason: String,
    commands: &SyncSender<ConnectorCommand>,
    events: &EventSender<DaemonEvent>,
) {
    kvlog::error!("attachment transfer failed", transfer_id, err = %reason);
    if commands
        .send(ConnectorCommand::CancelBulk(transfer_id))
        .is_err()
    {
        kvlog::error!("could not enqueue attachment cancellation", transfer_id);
    }
    if events
        .send_blocking(DaemonEvent::MediaTransferFailed {
            transfer_id,
            reason,
        })
        .is_err()
    {
        kvlog::error!("could not deliver attachment-failure event", transfer_id);
    }
}

fn writer_loop(
    commands: Receiver<ConnectorCommand>,
    stream: std::os::unix::net::UnixStream,
    events: EventSender<DaemonEvent>,
) -> Receiver<ConnectorCommand> {
    let mut writer = FrameWriter::new(stream);
    let mut prepared = HashMap::<RequestId, PreparedUpload>::new();
    let mut active = VecDeque::<ActiveUpload>::new();
    let mut chunk_bytes = local_rpc::MAX_CHUNK_BYTES;
    let mut internal_request_id = 1u64 << 63;
    loop {
        let command = match commands.try_recv() {
            Ok(command) => Some(command),
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) if active.is_empty() => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
            Err(TryRecvError::Empty) => None,
        };
        if let Some(command) = command {
            match command {
                ConnectorCommand::Frame(frame) => {
                    if let ClientFrame::CancelUpload { transfer_id, .. } = &frame {
                        prepared.retain(|_, upload| upload.upload.transfer_id != *transfer_id);
                        active.retain(|upload| upload.prepared.upload.transfer_id != *transfer_id);
                    }
                    #[cfg(feature = "diagnostic-logs")]
                    let attachment_request = match &frame {
                        ClientFrame::BeginAttachmentRead { request_id, read } => {
                            Some((*request_id, read.transfer_id, read.room_id))
                        }
                        _ => None,
                    };
                    if let Err(error) = writer.send_client(&frame) {
                        kvlog::error!(
                            "could not write daemon request",
                            request_id = frame.request_id(),
                            err = %error
                        );
                        let _ = writer.shutdown();
                        break;
                    }
                    #[cfg(feature = "diagnostic-logs")]
                    if let Some((request_id, transfer_id, room_id)) = attachment_request {
                        if crate::logger::rpc_logging_enabled() {
                            kvlog::info!(
                                "attachment request sent",
                                group = "daemon-rpc",
                                request_id,
                                transfer_id,
                                room_id
                            );
                        }
                    }
                }
                ConnectorCommand::PreparedUpload(upload) => {
                    let frame = ClientFrame::BeginUpload {
                        request_id: upload.begin_request,
                        upload: upload.upload.clone(),
                    };
                    if let Err(error) = writer.send_client(&frame) {
                        kvlog::error!(
                            "could not write begin-upload request",
                            request_id = upload.begin_request,
                            transfer_id = upload.upload.transfer_id,
                            err = %error
                        );
                        let _ = writer.shutdown();
                        break;
                    }
                    prepared.insert(upload.begin_request, upload);
                }
                ConnectorCommand::BeginUploadResult(result) => {
                    if events
                        .send_blocking(DaemonEvent::Frame(DaemonFrame::RequestResult(
                            result.clone(),
                        )))
                        .is_err()
                    {
                        let _ = writer.shutdown();
                        break;
                    }
                    let Some(upload) = prepared.remove(&result.request_id) else {
                        continue;
                    };
                    if matches!(result.outcome, RequestOutcome::Rejected { .. }) {
                        continue;
                    }
                    active.push_back(ActiveUpload {
                        prepared: upload,
                        buffer: vec![0; chunk_bytes],
                        sent: 0,
                    });
                }
                ConnectorCommand::ChunkBytes(bytes) => chunk_bytes = bytes,
                ConnectorCommand::CancelBulk(transfer_id) => {
                    let frame = ClientFrame::CancelBulkTransfer {
                        request_id: RequestId(internal_request_id),
                        transfer_id,
                    };
                    internal_request_id = internal_request_id.wrapping_add(1).max(1 << 63);
                    if let Err(error) = writer.send_client(&frame) {
                        kvlog::error!(
                            "could not write attachment cancellation",
                            transfer_id,
                            err = %error
                        );
                        let _ = writer.shutdown();
                        break;
                    }
                }
                ConnectorCommand::SessionEnded => break,
                ConnectorCommand::Retry => {}
            }
            continue;
        }

        let Some(mut upload) = active.pop_front() else {
            continue;
        };
        match stream_upload_chunk(&mut writer, &mut upload, chunk_bytes) {
            Ok(true) => active.push_back(upload),
            Ok(false) => {}
            Err(error) => {
                report_upload_error(
                    &events,
                    upload.prepared.begin_request,
                    upload.prepared.finish_request,
                    error,
                );
                let cancel_request = RequestId(internal_request_id);
                internal_request_id = internal_request_id.wrapping_add(1).max(1 << 63);
                let cancel = ClientFrame::CancelUpload {
                    request_id: cancel_request,
                    transfer_id: upload.prepared.upload.transfer_id,
                };
                if let Err(error) = writer.send_client(&cancel) {
                    kvlog::error!(
                        "could not write failed-upload cancellation",
                        request_id = cancel_request,
                        transfer_id = upload.prepared.upload.transfer_id,
                        err = %error
                    );
                    let _ = writer.shutdown();
                    break;
                }
            }
        }
    }
    commands
}

fn wait_for_retry(commands: &Receiver<ConnectorCommand>, duration: Duration) {
    match commands.recv_timeout(duration) {
        Ok(ConnectorCommand::Retry) => {}
        Ok(_) | Err(_) => {}
    }
}

/// Streams one chunk and returns whether the upload has more work.
fn stream_upload_chunk(
    writer: &mut FrameWriter,
    upload: &mut ActiveUpload,
    chunk_bytes: usize,
) -> Result<bool, String> {
    if chunk_bytes == 0 {
        return Err("daemon negotiated an empty upload chunk size".into());
    }
    if upload.buffer.len() != chunk_bytes {
        upload.buffer.resize(chunk_bytes, 0);
    }
    let read = loop {
        match upload.prepared.source.read(&mut upload.buffer) {
            Ok(read) => break read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    };
    if read != 0 {
        let next = upload
            .sent
            .checked_add(read as u64)
            .ok_or_else(|| "upload byte count overflow".to_string())?;
        if next > upload.prepared.upload.byte_len {
            return Err("upload exceeds its declared length while being read".into());
        }
        writer
            .send_client_bulk_chunk(upload.prepared.upload.transfer_id, &upload.buffer[..read])
            .map_err(|error| error.to_string())?;
        upload.sent = next;
        return Ok(true);
    }

    if upload.sent != upload.prepared.upload.byte_len {
        return Err("upload changed length while it was being read".into());
    }
    let frame = ClientFrame::FinishUpload {
        request_id: upload.prepared.finish_request,
        finished: BulkFinished {
            transfer_id: upload.prepared.upload.transfer_id,
        },
    };
    writer
        .send_client(&frame)
        .map_err(|error| error.to_string())?;
    Ok(false)
}

fn report_upload_error(
    events: &EventSender<DaemonEvent>,
    begin_request: RequestId,
    finish_request: RequestId,
    reason: String,
) {
    kvlog::error!(
        "upload failed",
        begin_request,
        finish_request,
        err = %reason
    );
    if events
        .send_blocking(DaemonEvent::UploadFailed {
            begin_request,
            finish_request,
            reason,
        })
        .is_err()
    {
        kvlog::error!(
            "could not deliver upload-failure event",
            begin_request,
            finish_request
        );
    }
}

fn drain_stale_commands(commands: &Receiver<ConnectorCommand>) {
    while commands.try_recv().is_ok() {}
}

#[cfg(test)]
mod tests {
    use std::{io::Write, os::unix::net::UnixStream};

    use super::*;

    #[test]
    fn upload_stream_reuses_buffer_and_sends_borrowed_bulk_chunks() {
        let source_bytes = b"abcdefghij";
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(source_bytes).unwrap();
        let transfer_id = BulkTransferId(17);
        let finish_request = RequestId(19);
        let mut upload = ActiveUpload {
            prepared: PreparedUpload {
                begin_request: RequestId(18),
                finish_request,
                upload: BeginUpload {
                    transfer_id,
                    room_id: local_rpc::ids::RoomId(3),
                    file_name: "upload.bin".into(),
                    byte_len: source_bytes.len() as u64,
                },
                source: UploadSource::File(source.reopen().unwrap()),
            },
            buffer: vec![0; 4],
            sent: 0,
        };
        let allocation = upload.buffer.as_ptr();
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(writer_stream);
        let mut reader = FrameReader::new(reader_stream);

        for expected in [b"abcd".as_slice(), b"efgh".as_slice(), b"ij".as_slice()] {
            assert!(stream_upload_chunk(&mut writer, &mut upload, 4).unwrap());
            assert_eq!(upload.buffer.as_ptr(), allocation);
            assert_eq!(upload.buffer.len(), 4);
            let mut handled = false;
            assert!(
                reader
                    .recv_client_with_bulk(|received_id, bytes| {
                        assert_eq!(received_id, transfer_id);
                        assert_eq!(bytes, expected);
                        handled = true;
                        Ok(())
                    })
                    .unwrap()
                    .is_none()
            );
            assert!(handled);
        }

        assert!(!stream_upload_chunk(&mut writer, &mut upload, 4).unwrap());
        assert_eq!(
            reader.recv_client().unwrap(),
            ClientFrame::FinishUpload {
                request_id: finish_request,
                finished: BulkFinished { transfer_id },
            }
        );
    }

    #[test]
    fn memory_upload_streams_directly_as_bulk_chunks() {
        let source_bytes = Arc::new(b"clipboard-image".to_vec());
        let transfer_id = BulkTransferId(27);
        let finish_request = RequestId(29);
        let mut upload = ActiveUpload {
            prepared: PreparedUpload {
                begin_request: RequestId(28),
                finish_request,
                upload: BeginUpload {
                    transfer_id,
                    room_id: local_rpc::ids::RoomId(4),
                    file_name: "pasted-image.png".into(),
                    byte_len: source_bytes.len() as u64,
                },
                source: UploadSource::Memory {
                    bytes: source_bytes,
                    offset: 0,
                },
            },
            buffer: vec![0; 5],
            sent: 0,
        };
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(writer_stream);
        let mut reader = FrameReader::new(reader_stream);

        for expected in [
            b"clipb".as_slice(),
            b"oard-".as_slice(),
            b"image".as_slice(),
        ] {
            assert!(stream_upload_chunk(&mut writer, &mut upload, 5).unwrap());
            let mut handled = false;
            assert!(
                reader
                    .recv_client_with_bulk(|received_id, bytes| {
                        assert_eq!(received_id, transfer_id);
                        assert_eq!(bytes, expected);
                        handled = true;
                        Ok(())
                    })
                    .unwrap()
                    .is_none()
            );
            assert!(handled);
        }

        assert!(!stream_upload_chunk(&mut writer, &mut upload, 5).unwrap());
        assert_eq!(
            reader.recv_client().unwrap(),
            ClientFrame::FinishUpload {
                request_id: finish_request,
                finished: BulkFinished { transfer_id },
            }
        );
    }
}
