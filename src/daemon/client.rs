use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rpc::daemon::{
    bulk::{BeginUpload, BulkChunk, BulkFinished},
    frame::{ClientFrame, ClientHello, DaemonFrame, Operation, RequestOutcome},
    model::{BulkTransferId, RequestId},
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
        request_id: RequestId,
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
    Retry,
}

struct PreparedUpload {
    begin_request: RequestId,
    finish_request: RequestId,
    upload: BeginUpload,
    path: PathBuf,
}

impl DaemonClient {
    pub fn spawn() -> (Self, Receiver<DaemonEvent>) {
        let (command_tx, command_rx) = mpsc::sync_channel(128);
        let (event_tx, event_rx) = mpsc::sync_channel(512);
        let connector_events = event_tx.clone();
        thread::Builder::new()
            .name("chatt-gui-daemon".into())
            .spawn(move || connection_loop(command_rx, connector_events))
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
    ) {
        let commands = self.commands.clone();
        let events = self.events.clone();
        let _ = thread::Builder::new()
            .name(format!("chatt-gui-upload-{}", transfer_id.0))
            .spawn(move || {
                let mut file = match File::open(&path) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = events.send(DaemonEvent::UploadPreparationFailed {
                            request_id: begin_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                let metadata = match file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = events.send(DaemonEvent::UploadPreparationFailed {
                            request_id: begin_request,
                            reason: error.to_string(),
                        });
                        return;
                    }
                };
                let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
                let mut buffer = vec![0; rpc::daemon::MAX_CHUNK_BYTES];
                loop {
                    let read = match file.read(&mut buffer) {
                        Ok(read) => read,
                        Err(error) => {
                            let _ = events.send(DaemonEvent::UploadPreparationFailed {
                                request_id: begin_request,
                                reason: error.to_string(),
                            });
                            return;
                        }
                    };
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                }
                let digest = digest.finish();
                let mut digest_bytes = [0; 32];
                digest_bytes.copy_from_slice(digest.as_ref());
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "upload".into());
                let upload = BeginUpload {
                    transfer_id,
                    room_id,
                    file_name,
                    byte_len: metadata.len(),
                    digest: digest_bytes,
                    content_type: "application/octet-stream".into(),
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
                        request_id: begin_request,
                        reason: "daemon connector stopped".into(),
                    });
                }
            });
    }

    pub fn retry(&self) {
        let _ = self.commands.try_send(ConnectorCommand::Retry);
    }
}

fn connection_loop(commands: Receiver<ConnectorCommand>, events: SyncSender<DaemonEvent>) {
    let mut delay = Duration::from_millis(250);
    loop {
        drain_stale_commands(&commands);
        let _ = events.try_send(DaemonEvent::Discovering);
        let hello = ClientHello::current(env!("CARGO_PKG_VERSION"), connection_nonce());
        let _ = events.try_send(DaemonEvent::Connecting);
        match rpc::daemon::unix::connect(&hello) {
            Ok(stream) => {
                delay = Duration::from_millis(250);
                let _ = events.try_send(DaemonEvent::TransportConnected);
                let reader_stream = match stream.try_clone() {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = events.try_send(DaemonEvent::Disconnected(error.to_string()));
                        continue;
                    }
                };
                let _ = reader_stream.set_read_timeout(Some(Duration::from_millis(100)));
                let mut reader = FrameReader::new(reader_stream);
                let mut writer = FrameWriter::new(stream);
                let mut uploads = HashMap::<RequestId, PreparedUpload>::new();
                let reason = 'connected: loop {
                    loop {
                        match commands.try_recv() {
                            Ok(ConnectorCommand::Frame(frame)) => {
                                let payload = match rpc::daemon::frame::encode_client(&frame) {
                                    Ok(payload) => payload,
                                    Err(error) => break 'connected error,
                                };
                                if let Err(error) = writer.send_payload(&payload, &[]) {
                                    break 'connected error.to_string();
                                }
                            }
                            Ok(ConnectorCommand::PreparedUpload(prepared)) => {
                                let frame = ClientFrame::BeginUpload {
                                    request_id: prepared.begin_request,
                                    upload: prepared.upload.clone(),
                                };
                                let payload = match rpc::daemon::frame::encode_client(&frame) {
                                    Ok(payload) => payload,
                                    Err(error) => break 'connected error,
                                };
                                if let Err(error) = writer.send_payload(&payload, &[]) {
                                    break 'connected error.to_string();
                                }
                                uploads.insert(prepared.begin_request, prepared);
                            }
                            Ok(ConnectorCommand::Retry) => {}
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    match reader.recv_payload() {
                        Ok(payload) => match rpc::daemon::frame::decode_daemon(&payload) {
                            Ok(frame) => {
                                if let DaemonFrame::RequestResult(result) = &frame
                                    && result.operation == Operation::BeginUpload
                                    && let Some(prepared) = uploads.remove(&result.request_id)
                                    && matches!(result.outcome, RequestOutcome::Accepted)
                                    && let Err(error) =
                                        stream_prepared_upload(&mut writer, prepared)
                                {
                                    break 'connected error;
                                }
                                if events.send(DaemonEvent::Frame(frame)).is_err() {
                                    return;
                                }
                            }
                            Err(error) => break error,
                        },
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => break error.to_string(),
                    }
                };
                let _ = events.try_send(DaemonEvent::Disconnected(reason));
            }
            Err(
                ConnectError::Incompatible(details)
                | ConnectError::Permission(details)
                | ConnectError::Rejected(details),
            ) => {
                let _ = events.try_send(DaemonEvent::Incompatible(details));
                wait_for_retry(&commands, Duration::from_secs(5));
            }
            Err(error) => {
                let _ = events.try_send(DaemonEvent::Disconnected(error.to_string()));
                wait_for_retry(&commands, delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

fn wait_for_retry(commands: &Receiver<ConnectorCommand>, duration: Duration) {
    match commands.recv_timeout(duration) {
        Ok(ConnectorCommand::Retry) => {}
        Ok(_) | Err(_) => {}
    }
}

fn stream_prepared_upload(
    writer: &mut FrameWriter,
    prepared: PreparedUpload,
) -> Result<(), String> {
    let mut file = File::open(&prepared.path).map_err(|error| error.to_string())?;
    let mut buffer = vec![0; rpc::daemon::MAX_CHUNK_BYTES];
    let mut offset = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        let frame = ClientFrame::UploadChunk(BulkChunk {
            transfer_id: prepared.upload.transfer_id,
            offset,
            bytes: buffer[..read].to_vec(),
        });
        let payload = rpc::daemon::frame::encode_client(&frame)?;
        writer
            .send_payload(&payload, &[])
            .map_err(|error| error.to_string())?;
        offset += read as u64;
    }
    let frame = ClientFrame::FinishUpload {
        request_id: prepared.finish_request,
        finished: BulkFinished {
            transfer_id: prepared.upload.transfer_id,
            byte_len: offset,
            digest: prepared.upload.digest,
        },
    };
    let payload = rpc::daemon::frame::encode_client(&frame)?;
    writer
        .send_payload(&payload, &[])
        .map_err(|error| error.to_string())
}

fn drain_stale_commands(commands: &Receiver<ConnectorCommand>) {
    while commands.try_recv().is_ok() {}
}

fn connection_nonce() -> [u8; 16] {
    let mut nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    for (index, byte) in std::process::id().to_le_bytes().into_iter().enumerate() {
        nonce[index] ^= byte;
    }
    nonce
}
