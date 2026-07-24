use std::{
    collections::HashMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use local_rpc::model::{AttachmentDescriptor, MediaKind};

const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Message {
    pub room_id: local_rpc::ids::RoomId,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCommandRow {
    pub local_id: u64,
    pub anchor_message_id: Option<u64>,
    pub body: String,
    pub error: bool,
    pub timestamp_ms: u64,
}

pub type CollapsedSections = HashMap<u64, Option<u64>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageListSource {
    Message {
        message_index: usize,
        message_id: u64,
    },
    Command {
        command_index: usize,
        local_id: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageListItem {
    pub source: MessageListSource,
    pub continuation: bool,
    pub collapsed_count: Option<usize>,
}

impl MessageListItem {
    pub fn is_collapsed(self) -> bool {
        self.collapsed_count.is_some()
    }

    pub fn has_same_visible_state(self, other: Self) -> bool {
        self.source == other.source
            && self.continuation == other.continuation
            && self.collapsed_count == other.collapsed_count
    }

    pub fn message_index(self) -> Option<usize> {
        match self.source {
            MessageListSource::Message { message_index, .. } => Some(message_index),
            MessageListSource::Command { .. } => None,
        }
    }

    pub fn message_id(self) -> Option<u64> {
        match self.source {
            MessageListSource::Message { message_id, .. } => Some(message_id),
            MessageListSource::Command { .. } => None,
        }
    }
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

pub fn from_daemon(message: local_rpc::model::Message) -> Message {
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
        && message.unverified == previous.unverified
        && message.sender == previous.sender
        && message.timestamp_ms.saturating_sub(previous.timestamp_ms) < GROUP_WINDOW_MS
}

/// Projects the flat feed into visible rows. Collapse markers inside a sender
/// group are sibling boundaries, so one collapsed range can never contain
/// another.
pub fn build_message_list(
    messages: &[Message],
    collapsed_sections: &CollapsedSections,
) -> Vec<MessageListItem> {
    let mut visible = Vec::new();

    for group_start in message_group_starts(messages) {
        let mut group_end = group_start + 1;
        while group_end < messages.len() && is_continuation(messages, group_end) {
            group_end += 1;
        }

        let mut sections = Vec::new();
        for index in group_start..group_end {
            let message_id = messages[index].id;
            let Some(end_id) = collapsed_sections.get(&message_id) else {
                continue;
            };
            let section_end = end_id
                .and_then(|end_id| {
                    (index + 1..group_end).find(|candidate| messages[*candidate].id == end_id)
                })
                .unwrap_or(group_end);
            sections.push((index, section_end));
        }

        // Group edits or prepended history can bring previously separate
        // sections together. Clamp each one at the next root boundary.
        for section_index in 0..sections.len() {
            let next_start = sections
                .get(section_index + 1)
                .map_or(group_end, |section| section.0);
            sections[section_index].1 = sections[section_index].1.min(next_start);
        }

        let mut section_index = 0;
        for index in group_start..group_end {
            while sections
                .get(section_index)
                .is_some_and(|section| index >= section.1)
            {
                section_index += 1;
            }
            let collapsed_section = sections
                .get(section_index)
                .filter(|section| index >= section.0 && index < section.1);
            if let Some((section_start, section_end)) = collapsed_section {
                if index != *section_start {
                    continue;
                }
                visible.push(MessageListItem {
                    source: MessageListSource::Message {
                        message_index: index,
                        message_id: messages[index].id,
                    },
                    continuation: false,
                    collapsed_count: Some(section_end - section_start),
                });
            } else {
                visible.push(MessageListItem {
                    source: MessageListSource::Message {
                        message_index: index,
                        message_id: messages[index].id,
                    },
                    continuation: index > group_start,
                    collapsed_count: None,
                });
            }
        }
    }

    visible
}

/// Merges ephemeral command output into the projected message feed. Command
/// rows stay after the remote tail that existed when the command completed.
pub fn build_timeline_list(
    messages: &[Message],
    command_rows: &[LocalCommandRow],
    collapsed_sections: &CollapsedSections,
) -> Vec<MessageListItem> {
    let remote = build_message_list(messages, collapsed_sections);
    let mut visible = Vec::with_capacity(remote.len() + command_rows.len());
    let mut inserted = vec![false; command_rows.len()];

    for item in remote {
        let message_id = item.message_id();
        visible.push(item);
        for (command_index, row) in command_rows.iter().enumerate() {
            if !inserted[command_index] && row.anchor_message_id == message_id {
                visible.push(MessageListItem {
                    source: MessageListSource::Command {
                        command_index,
                        local_id: row.local_id,
                    },
                    continuation: false,
                    collapsed_count: None,
                });
                inserted[command_index] = true;
            }
        }
    }

    for (command_index, row) in command_rows.iter().enumerate() {
        if !inserted[command_index] {
            visible.push(MessageListItem {
                source: MessageListSource::Command {
                    command_index,
                    local_id: row.local_id,
                },
                continuation: false,
                collapsed_count: None,
            });
        }
    }

    visible
}

/// Adds or removes a collapse marker rooted at `message_id`. A newly added
/// marker stops at the next existing marker in the same sender group.
pub fn toggle_collapsed_section(
    messages: &[Message],
    collapsed_sections: &mut CollapsedSections,
    message_id: u64,
) -> bool {
    if collapsed_sections.contains_key(&message_id) {
        collapsed_sections.remove(&message_id);
        return true;
    }

    let Some(selected) = messages.iter().position(|message| message.id == message_id) else {
        return false;
    };
    let mut group_end = selected + 1;
    while group_end < messages.len() && is_continuation(messages, group_end) {
        group_end += 1;
    }
    let next_section = (selected + 1..group_end)
        .map(|index| messages[index].id)
        .find(|candidate| collapsed_sections.contains_key(candidate));
    collapsed_sections.insert(message_id, next_section);
    true
}

fn message_group_starts(messages: &[Message]) -> impl Iterator<Item = usize> + '_ {
    (0..messages.len()).filter(|index| !is_continuation(messages, *index))
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
                id: local_rpc::model::AttachmentId {
                    timestamp_ms: 1,
                    transfer_id: local_rpc::ids::FileTransferId(1),
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
            room_id: local_rpc::ids::RoomId(1),
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
    fn collapses_from_selected_message_to_end_of_sender_group() {
        let messages = vec![
            message("Mara", 1_000),
            message("Mara", 2_000),
            message("Mara", 3_000),
            message("Ivo", 4_000),
        ];
        let mut sections = CollapsedSections::new();

        assert!(toggle_collapsed_section(&messages, &mut sections, 2_000));

        let visible = build_message_list(&messages, &sections);
        assert_eq!(
            visible
                .iter()
                .map(|item| (
                    item.message_id().unwrap(),
                    item.continuation,
                    item.collapsed_count
                ))
                .collect::<Vec<_>>(),
            vec![
                (1_000, false, None),
                (2_000, false, Some(2)),
                (4_000, false, None),
            ]
        );
    }

    #[test]
    fn collapsed_sections_split_at_the_next_root_instead_of_nesting() {
        let messages = vec![
            message("Mara", 1_000),
            message("Mara", 2_000),
            message("Mara", 3_000),
            message("Mara", 4_000),
        ];
        let mut sections = CollapsedSections::new();
        toggle_collapsed_section(&messages, &mut sections, 3_000);
        toggle_collapsed_section(&messages, &mut sections, 1_000);

        let visible = build_message_list(&messages, &sections);
        assert_eq!(
            visible
                .iter()
                .map(|item| (item.message_id().unwrap(), item.collapsed_count))
                .collect::<Vec<_>>(),
            vec![(1_000, Some(2)), (3_000, Some(2))]
        );

        toggle_collapsed_section(&messages, &mut sections, 1_000);
        let visible = build_message_list(&messages, &sections);
        assert_eq!(
            visible
                .iter()
                .map(|item| (
                    item.message_id().unwrap(),
                    item.continuation,
                    item.collapsed_count
                ))
                .collect::<Vec<_>>(),
            vec![
                (1_000, false, None),
                (2_000, true, None),
                (3_000, false, Some(2)),
            ]
        );
    }

    #[test]
    fn collapse_never_crosses_a_sender_group_boundary() {
        let messages = vec![
            message("Mara", 1_000),
            message("Mara", 2_000),
            message("Ivo", 3_000),
        ];
        let mut sections = CollapsedSections::new();
        toggle_collapsed_section(&messages, &mut sections, 1_000);

        let visible = build_message_list(&messages, &sections);
        assert_eq!(
            visible
                .iter()
                .map(|item| (item.message_id().unwrap(), item.collapsed_count))
                .collect::<Vec<_>>(),
            vec![(1_000, Some(2)), (3_000, None)]
        );
    }

    #[test]
    fn command_rows_remain_between_their_anchor_and_new_messages() {
        let messages = vec![message("Mara", 1_000), message("Ivo", 3_000)];
        let rows = vec![
            LocalCommandRow {
                local_id: 1,
                anchor_message_id: Some(1_000),
                body: "first".into(),
                error: false,
                timestamp_ms: 2_000,
            },
            LocalCommandRow {
                local_id: 2,
                anchor_message_id: Some(1_000),
                body: "second".into(),
                error: true,
                timestamp_ms: 2_001,
            },
        ];

        let visible = build_timeline_list(&messages, &rows, &CollapsedSections::new());
        assert_eq!(
            visible
                .iter()
                .map(|item| item.source)
                .collect::<Vec<_>>(),
            vec![
                MessageListSource::Message {
                    message_index: 0,
                    message_id: 1_000,
                },
                MessageListSource::Command {
                    command_index: 0,
                    local_id: 1,
                },
                MessageListSource::Command {
                    command_index: 1,
                    local_id: 2,
                },
                MessageListSource::Message {
                    message_index: 1,
                    message_id: 3_000,
                },
            ]
        );
    }

    #[test]
    fn command_rows_without_an_anchor_render_in_an_empty_feed() {
        let rows = vec![LocalCommandRow {
            local_id: 7,
            anchor_message_id: None,
            body: "ready".into(),
            error: false,
            timestamp_ms: 1,
        }];

        let visible = build_timeline_list(&[], &rows, &CollapsedSections::new());
        assert_eq!(
            visible[0].source,
            MessageListSource::Command {
                command_index: 0,
                local_id: 7,
            }
        );
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
