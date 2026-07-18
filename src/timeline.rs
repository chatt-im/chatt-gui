use std::{path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Message {
    pub id: u64,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub local: bool,
    pub edited: bool,
    pub attachment: Option<Attachment>,
}

#[derive(Clone, Debug)]
pub enum Attachment {
    Image {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    Video {
        path: PathBuf,
    },
}

impl Attachment {
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Image { path, .. } | Self::Video { path } => path,
        }
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
        && message.sender == previous.sender
        && message.timestamp_ms.saturating_sub(previous.timestamp_ms) < GROUP_WINDOW_MS
}

pub fn media_from_path(path: PathBuf) -> Option<Attachment> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();

    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "tga"
        | "ico" | "hdr" | "exr" | "pnm" | "qoi" => {
            let (width, height) = image::image_dimensions(&path).unwrap_or((4, 3));
            Some(Attachment::Image {
                path,
                width: width.max(1),
                height: height.max(1),
            })
        }
        "mp4" | "webm" | "mov" | "mkv" | "m4v" | "avi" => {
            Some(Attachment::Video { path })
        }
        _ => None,
    }
}

pub fn media_box_size(width: u32, height: u32) -> (f32, f32) {
    const MAX_WIDTH: f32 = 680.0;
    const MAX_HEIGHT: f32 = 420.0;
    const MIN_HEIGHT: f32 = 96.0;

    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let scale = (MAX_WIDTH / width).min(MAX_HEIGHT / height).min(1.0);
    let render_width = (width * scale).max(128.0).min(MAX_WIDTH);
    let render_height = (height * scale).max(MIN_HEIGHT).min(MAX_HEIGHT);
    (render_width, render_height)
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

pub fn sample_messages() -> Vec<Message> {
    let now = now_ms();
    let mut messages = Vec::with_capacity(240);
    let senders = ["Mara", "Theo", "You", "Inez", "Mara", "You"];
    let bodies = [
        "The native client can keep the room state close to the network loop.",
        "That should make the timeline feel immediate even with a long backlog.",
        "I’m matching the compact sender groups from the terminal view.",
        "Media keeps its measured space while pixels decode, so scroll position stays put.",
        "Drop an image or video anywhere in this window to add it to the room.",
        "The timeline only mounts rows near the viewport; this history is intentionally long.",
        "Once you scroll away from the end, incoming messages should not steal your place.",
        "The flat greys and restrained accents are borrowed from the web client.",
    ];

    for index in 0..240 {
        // Runs of three exercise the same compact sender grouping used by both
        // existing clients, while the whole feed is large enough to virtualize.
        let sender = senders[(index / 3) % senders.len()];
        messages.push(Message {
            id: index as u64 + 1,
            sender: sender.to_string(),
            body: bodies[index % bodies.len()].to_string(),
            timestamp_ms: now.saturating_sub((240 - index) as u64 * 48_000),
            local: sender == "You",
            edited: index == 52,
            attachment: None,
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(sender: &str, timestamp_ms: u64) -> Message {
        Message {
            id: timestamp_ms,
            sender: sender.to_string(),
            body: String::new(),
            timestamp_ms,
            local: false,
            edited: false,
            attachment: None,
        }
    }

    #[test]
    fn groups_adjacent_messages_from_the_same_sender() {
        let messages = vec![message("Mara", 1_000), message("Mara", 60_000)];
        assert!(!is_continuation(&messages, 0));
        assert!(is_continuation(&messages, 1));
    }

    #[test]
    fn edits_and_time_gaps_break_groups() {
        let mut messages = vec![message("Mara", 1_000), message("Mara", 60_000)];
        messages[1].edited = true;
        assert!(!is_continuation(&messages, 1));

        messages[1].edited = false;
        messages[1].timestamp_ms = 8 * 60 * 1000;
        assert!(!is_continuation(&messages, 1));
    }

    #[test]
    fn reserves_bounded_image_space() {
        assert_eq!(media_box_size(400, 300), (400.0, 300.0));
        assert_eq!(media_box_size(4_000, 3_000), (560.0, 420.0));
        assert_eq!(media_box_size(20, 20), (128.0, 96.0));
    }

    #[test]
    fn classifies_supported_media_extensions() {
        assert!(matches!(
            media_from_path(PathBuf::from("clip.MP4")),
            Some(Attachment::Video { .. })
        ));
        assert!(media_from_path(PathBuf::from("notes.txt")).is_none());
    }
}
