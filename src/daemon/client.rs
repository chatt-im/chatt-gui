use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Read,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
    time::Duration,
};

use async_channel::{Receiver as EventReceiver, Sender as EventSender};
use rpc::daemon::{
    bulk::{BeginUpload, BulkChunk, BulkFinished},
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
        stream_id: rpc::ids::StreamId,
        stream: std::os::unix::net::UnixStream,
    },
    Disconnected(String),
    Incompatible(String),
    UploadPreparationFailed {
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
    BeginUploadResult(rpc::daemon::frame::RequestResult),
    ChunkBytes(usize),
    CancelBulk(BulkTransferId),
    SessionEnded,
    Retry,
}

struct PreparedUpload {
    begin_request: RequestId,
    finish_request: RequestId,
    upload: BeginUpload,
    file: File,
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
                log::error!(
                    "could not enqueue daemon request request_id={:?}: {reason}",
                    request_id.map(|id| id.0),
                );
                reason
            })
    }

    pub fn upload_file(
        &self,
        path: PathBuf,
        room_id: rpc::ids::RoomId,
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
                        let _ = events.send_blocking(DaemonEvent::UploadPreparationFailed {
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
                        let _ = events.send_blocking(DaemonEvent::UploadPreparationFailed {
                            begin_request,
                            finish_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                if metadata.len() > max_upload_bytes {
                    let _ = events.send_blocking(DaemonEvent::UploadPreparationFailed {
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
                        file,
                    }))
                    .is_err()
                {
                    let _ = events.send_blocking(DaemonEvent::UploadPreparationFailed {
                        begin_request,
                        finish_request,
                        reason: "daemon connector stopped".into(),
                    });
                }
            })
        {
            let _ = spawn_errors.send(DaemonEvent::UploadPreparationFailed {
                begin_request,
                finish_request,
                reason: error.to_string(),
            });
        }
    }

    pub fn retry(&self) {
        let _ = self.commands.try_send(ConnectorCommand::Retry);
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
        match rpc::daemon::unix::connect(&hello) {
            Ok(stream) => {
                delay = Duration::from_millis(250);
                let _ = events.try_send(DaemonEvent::TransportConnected);
                let reader_stream = match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        log::error!("could not clone daemon RPC stream: {error}");
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
                        log::error!("could not start daemon RPC writer: {error}");
                        let _ = events.send_blocking(DaemonEvent::Disconnected(error.to_string()));
                        return;
                    }
                };
                let reason = 'connected: loop {
                    match reader.recv_daemon_with_fds() {
                        Ok(received) => {
                            let frame = received.frame;
                            let mut fds = received.fds;
                            if let DaemonFrame::LiveShareOpened {
                                request_id,
                                stream_id,
                            } = &frame
                            {
                                if fds.len() != 1 {
                                    break 'connected format!(
                                        "live share open carried {} descriptors instead of one",
                                        fds.len()
                                    );
                                }
                                let stream = std::os::unix::net::UnixStream::from(fds.pop().unwrap());
                                if events
                                    .send_blocking(DaemonEvent::LiveShareOpened {
                                        request_id: *request_id,
                                        stream_id: *stream_id,
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
                                    .unwrap_or(rpc::daemon::MAX_CHUNK_BYTES)
                                    .min(rpc::daemon::MAX_CHUNK_BYTES)
                                    .max(1);
                                if command_tx
                                    .send(ConnectorCommand::ChunkBytes(chunk_bytes))
                                    .is_err()
                                {
                                    break 'connected "daemon writer stopped".into();
                                }
                            }
                            if let DaemonFrame::RequestResult(result) = &frame
                                && result.operation == Operation::BeginUpload
                                && command_tx
                                    .send(ConnectorCommand::BeginUploadResult(result.clone()))
                                    .is_err()
                            {
                                break 'connected "daemon writer stopped".into();
                            }
                            if let DaemonFrame::RequestResult(result) = &frame {
                                match &result.outcome {
                                    RequestOutcome::Accepted => log::info!(
                                        "daemon request accepted request_id={} operation={:?}",
                                        result.request_id.0,
                                        result.operation,
                                    ),
                                    RequestOutcome::Rejected { code, message } => log::error!(
                                        "daemon request rejected request_id={} operation={:?} code={code}: {message}",
                                        result.request_id.0,
                                        result.operation,
                                    ),
                                }
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
                log::error!("daemon RPC connection ended: {reason}");
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
                log::error!("daemon RPC connection rejected: {details}");
                if events
                    .send_blocking(DaemonEvent::Incompatible(details))
                    .is_err()
                {
                    return;
                }
                wait_for_retry(&commands, Duration::from_secs(5));
            }
            Err(error) => {
                log::error!("daemon RPC connection failed: {error}");
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
            let transfer_id = chunk.transfer_id;
            if let Err(reason) = media_cache
                .lock()
                .expect("media cache lock poisoned")
                .chunk(chunk)
            {
                cancel_failed_download(transfer_id, reason, commands, events);
            }
            None
        }
        DaemonFrame::BulkFinished(finished) => {
            let transfer_id = finished.transfer_id;
            log::info!("attachment transfer finished transfer_id={}", transfer_id.0);
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
                        log::error!(
                            "could not deliver attachment-cached event transfer_id={}",
                            transfer_id.0,
                        );
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
            log::error!(
                "attachment transfer canceled transfer_id={}: {reason}",
                transfer_id.0,
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

fn cancel_failed_download(
    transfer_id: BulkTransferId,
    reason: String,
    commands: &SyncSender<ConnectorCommand>,
    events: &EventSender<DaemonEvent>,
) {
    log::error!(
        "attachment transfer failed transfer_id={}: {reason}",
        transfer_id.0,
    );
    if commands.send(ConnectorCommand::CancelBulk(transfer_id)).is_err() {
        log::error!(
            "could not enqueue attachment cancellation transfer_id={}",
            transfer_id.0,
        );
    }
    if events.send_blocking(DaemonEvent::MediaTransferFailed {
        transfer_id,
        reason,
    }).is_err() {
        log::error!(
            "could not deliver attachment-failure event transfer_id={}",
            transfer_id.0,
        );
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
    let mut chunk_bytes = rpc::daemon::MAX_CHUNK_BYTES;
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
                    let attachment_request = match &frame {
                        ClientFrame::BeginAttachmentRead { request_id, read } => Some((
                            request_id.0,
                            read.transfer_id.0,
                            read.room_id.0,
                        )),
                        _ => None,
                    };
                    if let Err(error) = writer.send_client(&frame) {
                        log::error!(
                            "could not write daemon request request_id={:?}: {error}",
                            frame.request_id().map(|id| id.0),
                        );
                        let _ = writer.shutdown();
                        break;
                    }
                    if let Some((request_id, transfer_id, room_id)) = attachment_request {
                        log::info!(
                            "attachment request sent request_id={request_id} transfer_id={transfer_id} room={room_id}"
                        );
                    }
                }
                ConnectorCommand::PreparedUpload(upload) => {
                    let frame = ClientFrame::BeginUpload {
                        request_id: upload.begin_request,
                        upload: upload.upload.clone(),
                    };
                    if let Err(error) = writer.send_client(&frame) {
                        log::error!(
                            "could not write begin-upload request request_id={} transfer_id={}: {error}",
                            upload.begin_request.0,
                            upload.upload.transfer_id.0,
                        );
                        let _ = writer.shutdown();
                        break;
                    }
                    prepared.insert(upload.begin_request, upload);
                }
                ConnectorCommand::BeginUploadResult(result) => {
                    let Some(upload) = prepared.remove(&result.request_id) else {
                        continue;
                    };
                    if let RequestOutcome::Rejected { message, .. } = result.outcome {
                        report_upload_error(
                            &events,
                            upload.begin_request,
                            upload.finish_request,
                            message,
                        );
                        continue;
                    }
                    active.push_back(ActiveUpload {
                        prepared: upload,
                        buffer: Vec::with_capacity(chunk_bytes),
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
                        log::error!(
                            "could not write attachment cancellation transfer_id={}: {error}",
                            transfer_id.0,
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
                let cancel = ClientFrame::CancelUpload {
                    request_id: upload.prepared.finish_request,
                    transfer_id: upload.prepared.upload.transfer_id,
                };
                if let Err(error) = writer.send_client(&cancel) {
                    log::error!(
                        "could not write failed-upload cancellation request_id={} transfer_id={}: {error}",
                        upload.prepared.finish_request.0,
                        upload.prepared.upload.transfer_id.0,
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
    upload.buffer.resize(chunk_bytes, 0);
    let read = upload
        .prepared
        .file
        .read(&mut upload.buffer)
        .map_err(|error| error.to_string())?;
    if read != 0 {
        upload.buffer.truncate(read);
        let chunk = BulkChunk {
            transfer_id: upload.prepared.upload.transfer_id,
            bytes: std::mem::take(&mut upload.buffer),
        };
        let frame = ClientFrame::UploadChunk(chunk);
        writer
            .send_client(&frame)
            .map_err(|error| error.to_string())?;
        let ClientFrame::UploadChunk(mut chunk) = frame else {
            unreachable!("constructed upload chunk frame")
        };
        upload.buffer = std::mem::take(&mut chunk.bytes);
        upload.sent += read as u64;
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
    log::error!(
        "upload failed begin_request={} finish_request={}: {reason}",
        begin_request.0,
        finish_request.0,
    );
    if events.send_blocking(DaemonEvent::UploadPreparationFailed {
        begin_request,
        finish_request,
        reason,
    }).is_err() {
        log::error!(
            "could not deliver upload-failure event begin_request={} finish_request={}",
            begin_request.0,
            finish_request.0,
        );
    }
}

fn drain_stale_commands(commands: &Receiver<ConnectorCommand>) {
    while commands.try_recv().is_ok() {}
}
