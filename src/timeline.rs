use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rpc::daemon::model::{AttachmentDescriptor, MediaKind};

const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Message {
    pub room_id: rpc::ids::RoomId,
    pub id: u64,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub local: bool,
    pub edited: bool,
    pub unverified: bool,
    pub notice: bool,
    pub attachment: Option<Attachment>,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub descriptor: AttachmentDescriptor,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.descriptor.media_kind == MediaKind::Image
    }
    pub fn is_video(&self) -> bool {
        self.descriptor.media_kind == MediaKind::Video
            || self.descriptor.content_type.starts_with("video/")
            || Path::new(&self.descriptor.file_name)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "ogv" | "webm"
                    )
                })
    }
}

pub fn from_daemon(message: rpc::daemon::model::Message) -> Message {
    Message {
        id: message.message_id.0,
        room_id: message.room_id,
        sender: message.sender_name,
        body: message.body,
        timestamp_ms: message.timestamp_ms,
        local: message.local,
        edited: message.edited,
        unverified: message.unverified,
        notice: message.notice,
        attachment: message
            .attachment
            .map(|descriptor| Attachment { descriptor }),
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn is_continuation(messages: &[Message], index: usize) -> bool {
    let Some(message) = messages.get(index) else {
        return false;
    };
    let Some(previous) = index.checked_sub(1).and_then(|index| messages.get(index)) else {
        return false;
    };
    !message.edited
        && !previous.edited
        && !message.notice
        && !previous.notice
        && message.sender == previous.sender
        && message.timestamp_ms.saturating_sub(previous.timestamp_ms) < GROUP_WINDOW_MS
}

pub fn media_box_size(width: u32, height: u32) -> (f32, f32) {
    const MAX_WIDTH: f32 = 680.0;
    const MAX_HEIGHT: f32 = 420.0;
    const MIN_HEIGHT: f32 = 96.0;
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let scale = (MAX_WIDTH / width).min(MAX_HEIGHT / height).min(1.0);
    (
        (width * scale).max(128.0).min(MAX_WIDTH),
        (height * scale).max(MIN_HEIGHT).min(MAX_HEIGHT),
    )
}

pub fn format_age(timestamp_ms: u64, current_ms: u64) -> String {
    let seconds = current_ms.saturating_sub(timestamp_ms) / 1000;
    match seconds {
        0..=44 => "now".to_string(),
        45..=89 => "1m".to_string(),
        90..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(file_name: &str, media_kind: MediaKind, content_type: &str) -> Attachment {
        Attachment {
            descriptor: AttachmentDescriptor {
                id: rpc::daemon::model::AttachmentId {
                    room_id: rpc::ids::RoomId(1),
                    message_id: rpc::ids::MessageId(1),
                },
                file_name: file_name.into(),
                media_kind,
                content_type: content_type.into(),
                byte_len: 1,
                width: None,
                height: None,
            },
        }
    }

    fn message(sender: &str, timestamp_ms: u64) -> Message {
        Message {
            room_id: rpc::ids::RoomId(1),
            id: timestamp_ms,
            sender: sender.into(),
            body: String::new(),
            timestamp_ms,
            local: false,
            edited: false,
            unverified: false,
            notice: false,
            attachment: None,
        }
    }

    #[test]
    fn groups_adjacent_messages_from_same_sender() {
        let messages = vec![message("Mara", 1_000), message("Mara", 60_000)];
        assert!(!is_continuation(&messages, 0));
        assert!(is_continuation(&messages, 1));
    }

    #[test]
    fn recognizes_playable_video_when_protocol_metadata_is_generic() {
        assert!(attachment("clip.MKV", MediaKind::File, "application/octet-stream").is_video());
        assert!(attachment("clip.bin", MediaKind::File, "video/mp4").is_video());
        assert!(!attachment("notes.txt", MediaKind::File, "text/plain").is_video());
    }

    #[test]
    fn reserves_bounded_image_space() {
        assert_eq!(media_box_size(400, 300), (400.0, 300.0));
        assert_eq!(media_box_size(4_000, 3_000), (560.0, 420.0));
    }
}
