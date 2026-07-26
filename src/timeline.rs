use std::{
    collections::HashMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local, NaiveDate, Utc};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentRenderKind {
    Image,
    Audio,
    Video,
    Other,
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
    pub day_separator: bool,
    pub trailing_gap: bool,
}

impl MessageListItem {
    pub fn is_collapsed(self) -> bool {
        self.collapsed_count.is_some()
    }

    pub fn has_same_visible_state(self, other: Self) -> bool {
        self.source == other.source
            && self.continuation == other.continuation
            && self.collapsed_count == other.collapsed_count
            && self.day_separator == other.day_separator
            && self.trailing_gap == other.trailing_gap
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
    pub fn render_kind(&self) -> AttachmentRenderKind {
        match self.descriptor.media_kind {
            MediaKind::Image => AttachmentRenderKind::Image,
            MediaKind::Audio => AttachmentRenderKind::Audio,
            MediaKind::Video => AttachmentRenderKind::Video,
            MediaKind::File => {
                if self.descriptor.content_type.starts_with("audio/") {
                    AttachmentRenderKind::Audio
                } else if self.descriptor.content_type.starts_with("video/") {
                    AttachmentRenderKind::Video
                } else if has_extension(
                    &self.descriptor.file_name,
                    &[
                        "aac", "ac3", "aif", "aifc", "aiff", "eac3", "ec3", "flac", "m4a", "mka",
                        "mp3", "oga", "ogg", "opus", "wav", "weba",
                    ],
                ) {
                    AttachmentRenderKind::Audio
                } else if has_extension(
                    &self.descriptor.file_name,
                    &["avi", "m4v", "mkv", "mov", "mp4", "ogv", "webm"],
                ) {
                    AttachmentRenderKind::Video
                } else {
                    AttachmentRenderKind::Other
                }
            }
        }
    }

    pub fn is_image(&self) -> bool {
        self.render_kind() == AttachmentRenderKind::Image
    }

    pub fn is_audio(&self) -> bool {
        self.render_kind() == AttachmentRenderKind::Audio
    }

    pub fn is_video(&self) -> bool {
        self.render_kind() == AttachmentRenderKind::Video
    }
}

fn has_extension(file_name: &str, candidates: &[&str]) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            candidates
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
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
                    day_separator: false,
                    trailing_gap: false,
                });
            } else {
                visible.push(MessageListItem {
                    source: MessageListSource::Message {
                        message_index: index,
                        message_id: messages[index].id,
                    },
                    continuation: index > group_start,
                    collapsed_count: None,
                    day_separator: false,
                    trailing_gap: false,
                });
            }
        }
    }

    let mut previous_date = None;
    for item in &mut visible {
        let Some(message_index) = item.message_index() else {
            continue;
        };
        let date = local_date(messages[message_index].timestamp_ms);
        item.day_separator = date.is_some() && date != previous_date;
        if date.is_some() {
            previous_date = date;
        }
    }

    mark_trailing_gaps(&mut visible);
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
                    day_separator: false,
                    trailing_gap: false,
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
                day_separator: false,
                trailing_gap: false,
            });
        }
    }

    mark_trailing_gaps(&mut visible);
    visible
}

fn mark_trailing_gaps(items: &mut [MessageListItem]) {
    for index in 0..items.len() {
        let continues_message = matches!(items[index].source, MessageListSource::Message { .. })
            && items.get(index + 1).is_some_and(|next| {
                matches!(next.source, MessageListSource::Message { .. }) && next.continuation
            });
        items[index].trailing_gap = !continues_message;
    }
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

/// Removes the collapse marker hiding `message_id`, if any.
pub fn reveal_message(
    messages: &[Message],
    collapsed_sections: &mut CollapsedSections,
    message_id: u64,
) -> bool {
    let Some(target_index) = messages.iter().position(|message| message.id == message_id) else {
        return false;
    };
    let containing_root = collapsed_sections.iter().find_map(|(root_id, end_id)| {
        let root_index = messages.iter().position(|message| message.id == *root_id)?;
        let mut group_end = root_index + 1;
        while group_end < messages.len() && is_continuation(messages, group_end) {
            group_end += 1;
        }
        let section_end = end_id
            .and_then(|end_id| {
                (root_index + 1..group_end).find(|index| messages[*index].id == end_id)
            })
            .unwrap_or(group_end);
        (target_index >= root_index && target_index < section_end).then_some(*root_id)
    });
    containing_root.is_some_and(|root_id| collapsed_sections.remove(&root_id).is_some())
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

pub fn format_day_label(timestamp_ms: u64, current_ms: u64) -> Option<String> {
    let date = local_date(timestamp_ms)?;
    let today = local_date(current_ms)?;
    Some(format_day_label_for_dates(date, today))
}

fn local_date(timestamp_ms: u64) -> Option<NaiveDate> {
    let timestamp_ms = i64::try_from(timestamp_ms).ok()?;
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
}

fn format_day_label_for_dates(date: NaiveDate, today: NaiveDate) -> String {
    if date == today {
        "Today".to_string()
    } else if today.pred_opt() == Some(date) {
        "Yesterday".to_string()
    } else {
        date.format("%B %-d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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

    fn local_timestamp_ms(year: i32, month: u32, day: u32, hour: u32) -> u64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("test timestamp should be unambiguous")
            .timestamp_millis() as u64
    }

    #[test]
    fn groups_adjacent_messages_from_same_sender() {
        let messages = vec![message("Mara", 1_000), message("Mara", 60_000)];
        assert!(!is_continuation(&messages, 0));
        assert!(is_continuation(&messages, 1));
    }

    #[test]
    fn adds_trailing_space_only_at_visible_group_boundaries() {
        let messages = vec![
            message("Mara", 1_000),
            message("Mara", 2_000),
            message("Ivo", 3_000),
        ];

        let visible = build_message_list(&messages, &CollapsedSections::new());

        assert_eq!(
            visible
                .iter()
                .map(|item| item.trailing_gap)
                .collect::<Vec<_>>(),
            vec![false, true, true]
        );
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
            visible.iter().map(|item| item.source).collect::<Vec<_>>(),
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
    fn marks_first_visible_message_and_local_day_changes() {
        let first_day = local_timestamp_ms(2026, 7, 24, 12);
        let same_day = local_timestamp_ms(2026, 7, 24, 18);
        let next_day = local_timestamp_ms(2026, 7, 25, 12);
        let messages = vec![
            message("Mara", first_day),
            message("Ivo", same_day),
            message("Mara", next_day),
        ];

        let visible = build_message_list(&messages, &CollapsedSections::new());

        assert_eq!(
            visible
                .iter()
                .map(|item| item.day_separator)
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
    }

    #[test]
    fn collapsed_roots_keep_visible_day_boundaries() {
        let first_day = local_timestamp_ms(2026, 7, 24, 23);
        let next_day = local_timestamp_ms(2026, 7, 25, 0);
        let next_day_continuation = next_day + 60_000;
        let messages = vec![
            message("Mara", first_day),
            message("Mara", next_day),
            message("Mara", next_day_continuation),
        ];
        let mut collapsed = CollapsedSections::new();
        toggle_collapsed_section(&messages, &mut collapsed, next_day);

        let visible = build_message_list(&messages, &collapsed);

        assert_eq!(visible.len(), 2);
        assert!(visible[0].day_separator);
        assert!(visible[1].day_separator);
        assert_eq!(visible[1].collapsed_count, Some(2));
    }

    #[test]
    fn command_rows_do_not_interrupt_message_day_boundaries() {
        let first_day = local_timestamp_ms(2026, 7, 24, 12);
        let next_day = local_timestamp_ms(2026, 7, 25, 12);
        let messages = vec![message("Mara", first_day), message("Ivo", next_day)];
        let rows = vec![LocalCommandRow {
            local_id: 1,
            anchor_message_id: Some(first_day),
            body: "done".into(),
            error: false,
            timestamp_ms: first_day,
        }];

        let visible = build_timeline_list(&messages, &rows, &CollapsedSections::new());

        assert_eq!(
            visible
                .iter()
                .map(|item| item.day_separator)
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
    }

    #[test]
    fn formats_relative_and_long_day_labels() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 25).unwrap();
        let yesterday = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();
        let older = NaiveDate::from_ymd_opt(2025, 12, 3).unwrap();

        assert_eq!(format_day_label_for_dates(today, today), "Today");
        assert_eq!(format_day_label_for_dates(yesterday, today), "Yesterday");
        assert_eq!(format_day_label_for_dates(older, today), "December 3, 2025");
    }

    #[test]
    fn recognizes_playable_video_when_protocol_metadata_is_generic() {
        assert!(attachment("clip.MKV", MediaKind::File, "application/octet-stream").is_video());
        assert!(attachment("clip.bin", MediaKind::File, "video/mp4").is_video());
        assert!(!attachment("notes.txt", MediaKind::File, "text/plain").is_video());
    }

    #[test]
    fn recognizes_common_audio_when_protocol_metadata_is_generic() {
        for extension in [
            "aac", "ac3", "aif", "aifc", "aiff", "eac3", "ec3", "flac", "m4a", "mka", "mp3", "oga",
            "ogg", "opus", "wav", "weba",
        ] {
            assert!(
                attachment(
                    &format!("recording.{extension}"),
                    MediaKind::File,
                    "application/octet-stream",
                )
                .is_audio(),
                "extension {extension} should route to audio playback",
            );
        }
        assert!(
            attachment("recording.MP3", MediaKind::File, "application/octet-stream").is_audio()
        );
        assert!(attachment("recording.bin", MediaKind::File, "audio/mpeg").is_audio());
        assert!(
            attachment(
                "recording.bin",
                MediaKind::Audio,
                "application/octet-stream"
            )
            .is_audio()
        );
        assert!(!attachment("notes.txt", MediaKind::File, "text/plain").is_audio());
    }

    #[test]
    fn authoritative_media_metadata_keeps_audio_and_video_routes_exclusive() {
        let video = attachment("clip.ogg", MediaKind::Video, "video/ogg");
        assert_eq!(video.render_kind(), AttachmentRenderKind::Video);
        assert!(video.is_video());
        assert!(!video.is_audio());

        let audio = attachment("recording.mp4", MediaKind::Audio, "audio/mp4");
        assert_eq!(audio.render_kind(), AttachmentRenderKind::Audio);
        assert!(audio.is_audio());
        assert!(!audio.is_video());

        let generic_video = attachment("clip.ogg", MediaKind::File, "video/ogg");
        assert_eq!(generic_video.render_kind(), AttachmentRenderKind::Video);
    }

    #[test]
    fn reserves_bounded_image_space() {
        assert_eq!(media_box_size(400, 300), (400.0, 300.0));
        assert_eq!(media_box_size(4_000, 3_000), (560.0, 420.0));
    }

    #[test]
    fn revealing_a_hidden_message_removes_its_containing_collapse() {
        let messages = vec![
            message("Mara", 1_000),
            message("Mara", 2_000),
            message("Mara", 3_000),
        ];
        let mut sections = CollapsedSections::new();
        toggle_collapsed_section(&messages, &mut sections, 1_000);

        assert!(reveal_message(&messages, &mut sections, 2_000));
        assert!(sections.is_empty());
        assert_eq!(build_message_list(&messages, &sections).len(), 3);
    }
}
