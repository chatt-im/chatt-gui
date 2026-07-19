use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Read,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
    time::Duration,
};

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
    Disconnected(String),
    Incompatible(String),
    UploadPreparationFailed {
        begin_request: RequestId,
        finish_request: RequestId,
        reason: String,
    },
    MediaTransferStarted,
    MediaCached(AttachmentDescriptor),
    MediaTransferFailed {
        transfer_id: BulkTransferId,
        reason: String,
    },
}

#[derive(Clone)]
pub struct DaemonClient {
    commands: SyncSender<ConnectorCommand>,
    events: SyncSender<DaemonEvent>,
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
    path: PathBuf,
}

struct ActiveUpload {
    prepared: PreparedUpload,
    file: File,
    buffer: Vec<u8>,
    offset: u64,
    digest: aws_lc_rs::digest::Context,
}

impl DaemonClient {
    pub fn spawn(
        media_cache: std::sync::Arc<std::sync::Mutex<crate::media_cache::MediaCache>>,
    ) -> (Self, Receiver<DaemonEvent>) {
        let (command_tx, command_rx) = mpsc::sync_channel(128);
        let (event_tx, event_rx) = mpsc::sync_channel(64);
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
        self.commands
            .try_send(ConnectorCommand::Frame(frame))
            .map_err(|error| match error {
                TrySendError::Full(_) => "daemon command queue is full".into(),
                TrySendError::Disconnected(_) => "daemon connector stopped".into(),
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
                        let _ = events.send(DaemonEvent::UploadPreparationFailed {
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
                        let _ = events.send(DaemonEvent::UploadPreparationFailed {
                            begin_request,
                            finish_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                if metadata.len() > max_upload_bytes {
                    let _ = events.send(DaemonEvent::UploadPreparationFailed {
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
                        path,
                    }))
                    .is_err()
                {
                    let _ = events.send(DaemonEvent::UploadPreparationFailed {
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
    events: SyncSender<DaemonEvent>,
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
                        if events
                            .send(DaemonEvent::Disconnected(error.to_string()))
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
                        let _ = events.send(DaemonEvent::Disconnected(error.to_string()));
                        return;
                    }
                };
                let reason = 'connected: loop {
                    match reader.recv_daemon() {
                        Ok(frame) => {
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
                            if matches!(
                                &frame,
                                DaemonFrame::RequestResult(result)
                                    if result.request_id.0 & (1 << 63) != 0
                            ) {
                                continue;
                            }
                            if events.send(DaemonEvent::Frame(frame)).is_err() {
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
                if events.send(DaemonEvent::Disconnected(reason)).is_err() {
                    return;
                }
            }
            Err(
                ConnectError::Incompatible(details)
                | ConnectError::Permission(details)
                | ConnectError::Rejected(details),
            ) => {
                if events.send(DaemonEvent::Incompatible(details)).is_err() {
                    return;
                }
                wait_for_retry(&commands, Duration::from_secs(5));
            }
            Err(error) => {
                if events
                    .send(DaemonEvent::Disconnected(error.to_string()))
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
    events: &SyncSender<DaemonEvent>,
) -> Option<DaemonFrame> {
    match frame {
        DaemonFrame::BulkStarted(started) => {
            let transfer_id = started.transfer_id;
            match media_cache
                .lock()
                .expect("media cache lock poisoned")
                .begin(started)
            {
                Ok(()) => {
                    let _ = events.send(DaemonEvent::MediaTransferStarted);
                }
                Err(reason) => cancel_failed_download(transfer_id, reason, commands, events),
            }
            None
        }
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
            match media_cache
                .lock()
                .expect("media cache lock poisoned")
                .finish(finished)
            {
                Ok(descriptor) => {
                    let _ = events.send(DaemonEvent::MediaCached(descriptor));
                }
                Err(reason) => cancel_failed_download(transfer_id, reason, commands, events),
            }
            None
        }
        DaemonFrame::BulkCanceled {
            transfer_id,
            reason,
        } => {
            media_cache
                .lock()
                .expect("media cache lock poisoned")
                .cancel(transfer_id);
            let _ = events.send(DaemonEvent::MediaTransferFailed {
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
    events: &SyncSender<DaemonEvent>,
) {
    let _ = commands.send(ConnectorCommand::CancelBulk(transfer_id));
    let _ = events.send(DaemonEvent::MediaTransferFailed {
        transfer_id,
        reason,
    });
}

fn writer_loop(
    commands: Receiver<ConnectorCommand>,
    stream: std::os::unix::net::UnixStream,
    events: SyncSender<DaemonEvent>,
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
                    if writer.send_client(&frame).is_err() {
                        let _ = writer.shutdown();
                        break;
                    }
                }
                ConnectorCommand::PreparedUpload(upload) => {
                    let frame = ClientFrame::BeginUpload {
                        request_id: upload.begin_request,
                        upload: upload.upload.clone(),
                    };
                    if writer.send_client(&frame).is_err() {
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
                    match File::open(&upload.path) {
                        Ok(file) => active.push_back(ActiveUpload {
                            prepared: upload,
                            file,
                            buffer: Vec::with_capacity(chunk_bytes),
                            offset: 0,
                            digest: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
                        }),
                        Err(error) => {
                            report_upload_error(
                                &events,
                                upload.begin_request,
                                upload.finish_request,
                                error.to_string(),
                            );
                            if writer
                                .send_client(&ClientFrame::CancelUpload {
                                    request_id: upload.finish_request,
                                    transfer_id: upload.upload.transfer_id,
                                })
                                .is_err()
                            {
                                let _ = writer.shutdown();
                                break;
                            }
                        }
                    }
                }
                ConnectorCommand::ChunkBytes(bytes) => chunk_bytes = bytes,
                ConnectorCommand::CancelBulk(transfer_id) => {
                    let frame = ClientFrame::CancelBulkTransfer {
                        request_id: RequestId(internal_request_id),
                        transfer_id,
                    };
                    internal_request_id = internal_request_id.wrapping_add(1).max(1 << 63);
                    if writer.send_client(&frame).is_err() {
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
                if writer
                    .send_client(&ClientFrame::CancelUpload {
                        request_id: upload.prepared.finish_request,
                        transfer_id: upload.prepared.upload.transfer_id,
                    })
                    .is_err()
                {
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
        .file
        .read(&mut upload.buffer)
        .map_err(|error| error.to_string())?;
    if read != 0 {
        upload.buffer.truncate(read);
        upload.digest.update(&upload.buffer);
        let chunk = BulkChunk {
            transfer_id: upload.prepared.upload.transfer_id,
            offset: upload.offset,
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
        upload.offset += read as u64;
        return Ok(true);
    }

    if upload.offset != upload.prepared.upload.byte_len {
        return Err("upload changed length while it was being read".into());
    }
    let digest = std::mem::replace(
        &mut upload.digest,
        aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
    )
    .finish();
    let mut digest_bytes = [0; 32];
    digest_bytes.copy_from_slice(digest.as_ref());
    let frame = ClientFrame::FinishUpload {
        request_id: upload.prepared.finish_request,
        finished: BulkFinished {
            transfer_id: upload.prepared.upload.transfer_id,
            byte_len: upload.offset,
            digest: digest_bytes,
        },
    };
    writer
        .send_client(&frame)
        .map_err(|error| error.to_string())?;
    Ok(false)
}

fn report_upload_error(
    events: &SyncSender<DaemonEvent>,
    begin_request: RequestId,
    finish_request: RequestId,
    reason: String,
) {
    let _ = events.send(DaemonEvent::UploadPreparationFailed {
        begin_request,
        finish_request,
        reason,
    });
}

fn drain_stale_commands(commands: &Receiver<ConnectorCommand>) {
    while commands.try_recv().is_ok() {}
}
