use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::LazyLock,
};

use unicode_segmentation::UnicodeSegmentation;

const DATABASE_BYTES: &[u8] = include_bytes!("../assets/emoji.db");
const DATABASE_MAGIC: &[u8; 4] = b"TE02";
const MAX_SHORTCODE_UNITS: usize = 64;
const EMOJI_GROUPS: [u8; 9] = [0, 1, 3, 4, 5, 6, 7, 8, 9];
const NAME_TOKENS: [&str; 32] = [
    "_face",
    "woman",
    "person",
    "_with_",
    "family",
    "heart",
    "right",
    "arrow",
    "hand",
    "white",
    "square",
    "black",
    "small",
    "moon",
    "wheelchair",
    "left",
    "closed",
    "eyes",
    "worker",
    "medium",
    "light",
    "dark",
    "button",
    "circle",
    "open",
    "baby",
    "running",
    "walking",
    "haired",
    "man",
    "flag",
    "_of_",
];
const POINT_TOKENS: [u32; 8] = [
    0xfe0f, 0x200d, 0x2642, 0x2640, 0x1f466, 0x1f467, 0x27a1, 0x1f469,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EmojiRecord {
    pub id: usize,
    pub unicode: String,
    pub shortcode: String,
    pub label: String,
    group: u8,
    search: String,
}

#[derive(Debug)]
struct EmojiDatabase {
    emoji: Vec<EmojiRecord>,
    by_shortcode: HashMap<String, usize>,
    by_unicode: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShortcodeRange<'a> {
    pub range: Range<usize>,
    pub shortcode: &'a str,
}

static DATABASE: LazyLock<EmojiDatabase> = LazyLock::new(|| {
    decode_database(DATABASE_BYTES).expect("embedded emoji database must be valid")
});

pub(crate) fn exact_shortcode(shortcode: &str) -> Option<&'static EmojiRecord> {
    let id = *DATABASE.by_shortcode.get(shortcode)?;
    DATABASE.emoji.get(id)
}

pub(crate) fn suggestions(query: &str, limit: usize) -> Vec<&'static EmojiRecord> {
    if limit == 0 {
        return Vec::new();
    }
    if query.is_empty() {
        return DATABASE
            .emoji
            .iter()
            .filter(|record| record.group == 0)
            .take(limit)
            .collect();
    }

    let normalized = normalize_query(query);
    if normalized.is_empty() {
        return Vec::new();
    }
    let code_query = normalized.split(' ').collect::<Vec<_>>().join("_");
    let exact = DATABASE.by_shortcode.get(&code_query).copied();
    let mut matches = Vec::<(u8, usize)>::with_capacity(limit);

    for record in &DATABASE.emoji {
        let rank = if exact == Some(record.id) {
            0
        } else if record.shortcode.starts_with(&code_query) {
            1
        } else if record.search.starts_with(&normalized) {
            2
        } else if record.search.contains(&normalized) {
            3
        } else {
            continue;
        };
        let candidate = (rank, record.id);
        let position = matches
            .iter()
            .position(|current| compare_match(candidate, *current) == Ordering::Less)
            .unwrap_or(matches.len());
        if position < limit {
            matches.insert(position, candidate);
            if matches.len() > limit {
                matches.pop();
            }
        }
    }

    matches
        .into_iter()
        .filter_map(|(_, id)| DATABASE.emoji.get(id))
        .collect()
}

pub(crate) fn emoji_only_count(value: &str) -> Option<usize> {
    let mut count = 0;
    for grapheme in value.graphemes(true) {
        if grapheme.chars().all(char::is_whitespace) {
            continue;
        }
        if !DATABASE.by_unicode.contains(grapheme) {
            return None;
        }
        count += 1;
    }
    (count > 0).then_some(count)
}

fn compare_match(left: (u8, usize), right: (u8, usize)) -> Ordering {
    let left_record = &DATABASE.emoji[left.1];
    let right_record = &DATABASE.emoji[right.1];
    left.0
        .cmp(&right.0)
        .then_with(|| {
            left_record
                .shortcode
                .len()
                .cmp(&right_record.shortcode.len())
        })
        .then_with(|| left.1.cmp(&right.1))
}

fn normalize_query(query: &str) -> String {
    let query = query
        .strip_prefix(':')
        .unwrap_or(query)
        .strip_suffix(':')
        .unwrap_or_else(|| query.strip_prefix(':').unwrap_or(query));
    let mut normalized = String::new();
    let mut separator = false;
    for character in query.to_lowercase().chars() {
        if matches!(character, '_' | '-') {
            if !separator {
                normalized.push(' ');
                separator = true;
            }
        } else {
            normalized.push(character);
            separator = false;
        }
    }
    normalized.trim().to_string()
}

pub(crate) fn find_colon_trigger(
    value: &str,
    selection: Range<usize>,
) -> Option<ShortcodeRange<'_>> {
    if selection.start != selection.end
        || selection.end > value.len()
        || !value.is_char_boundary(selection.end)
    {
        return None;
    }
    let query_start = token_start(value, selection.end)?;
    if query_start == 0 || value.as_bytes().get(query_start - 1) != Some(&b':') {
        return None;
    }
    let colon = query_start - 1;
    if !has_valid_boundary(value, colon) || is_in_markdown_code(value, colon) {
        return None;
    }
    Some(ShortcodeRange {
        range: colon..selection.end,
        shortcode: &value[query_start..selection.end],
    })
}

pub(crate) fn find_completed_shortcode(value: &str, caret: usize) -> Option<ShortcodeRange<'_>> {
    if caret == 0
        || caret > value.len()
        || !value.is_char_boundary(caret)
        || value.as_bytes().get(caret - 1) != Some(&b':')
    {
        return None;
    }
    let query_end = caret - 1;
    let query_start = token_start(value, query_end)?;
    if query_start == query_end
        || query_start == 0
        || value.as_bytes().get(query_start - 1) != Some(&b':')
    {
        return None;
    }
    let colon = query_start - 1;
    if !has_valid_boundary(value, colon) || is_in_markdown_code(value, colon) {
        return None;
    }
    Some(ShortcodeRange {
        range: colon..caret,
        shortcode: &value[query_start..query_end],
    })
}

fn token_start(value: &str, end: usize) -> Option<usize> {
    if end > value.len() || !value.is_char_boundary(end) {
        return None;
    }
    let mut cursor = end;
    let mut units = 0;
    while cursor > 0 {
        let (start, character) = value[..cursor].char_indices().next_back()?;
        if character != '_' && !character.is_alphanumeric() {
            break;
        }
        units += character.len_utf16();
        if units > MAX_SHORTCODE_UNITS {
            return None;
        }
        cursor = start;
    }
    Some(cursor)
}

fn has_valid_boundary(value: &str, colon: usize) -> bool {
    if colon == 0 {
        return true;
    }
    value[..colon].chars().next_back().is_some_and(|character| {
        character != ':' && character != '_' && !character.is_alphanumeric()
    })
}

fn backtick_run(value: &str, start: usize, end: usize) -> usize {
    let bytes = value.as_bytes();
    let mut cursor = start;
    while cursor < end && bytes[cursor] == b'`' {
        cursor += 1;
    }
    cursor - start
}

fn is_escaped(value: &str, mut offset: usize) -> bool {
    let bytes = value.as_bytes();
    let mut slashes = 0;
    while offset > 0 && bytes[offset - 1] == b'\\' {
        offset -= 1;
        slashes += 1;
    }
    slashes % 2 == 1
}

fn is_in_markdown_code(value: &str, offset: usize) -> bool {
    let bytes = value.as_bytes();
    let mut line_start = 0;
    let mut fence_ticks = 0;
    let mut inline_ticks = 0;

    while line_start <= offset {
        let newline = value[line_start..]
            .find('\n')
            .map(|index| line_start + index);
        let line_end = newline.unwrap_or(value.len());
        let scan_end = offset.min(line_end);
        let mut content_start = line_start;
        while content_start < line_end
            && content_start - line_start < 3
            && bytes[content_start] == b' '
        {
            content_start += 1;
        }
        let ticks = backtick_run(value, content_start, line_end);

        if fence_ticks > 0 {
            if offset <= line_end {
                return true;
            }
            let tail = &value[content_start + ticks..line_end];
            if ticks >= fence_ticks && tail.trim().is_empty() {
                fence_ticks = 0;
            }
        } else if inline_ticks == 0 && ticks >= 3 {
            fence_ticks = ticks;
            if offset <= line_end {
                return true;
            }
        } else {
            let mut cursor = line_start;
            while cursor < scan_end {
                if bytes[cursor] != b'`' || (inline_ticks == 0 && is_escaped(value, cursor)) {
                    cursor += 1;
                    continue;
                }
                let run = backtick_run(value, cursor, scan_end);
                if inline_ticks == 0 {
                    inline_ticks = run;
                } else if run == inline_ticks {
                    inline_ticks = 0;
                }
                cursor += run;
            }
            if offset <= line_end {
                return inline_ticks > 0;
            }
        }

        let Some(newline) = newline else {
            break;
        };
        line_start = newline + 1;
    }
    false
}

fn decode_database(bytes: &[u8]) -> Result<EmojiDatabase, String> {
    if bytes.get(..4) != Some(DATABASE_MAGIC) {
        return Err("invalid emoji database magic".into());
    }
    if bytes.len() < 26 {
        return Err("truncated emoji database header".into());
    }
    let record_count = read_u16(bytes, 4)? as usize;
    let flag_count = read_u16(bytes, 6)? as usize;
    let glyph_length = read_u32(bytes, 8)? as usize;
    let name_length = read_u32(bytes, 12)? as usize;
    let group_length = read_u16(bytes, 16)? as usize;
    let flag_length = read_u16(bytes, 18)? as usize;
    let tone_length = read_u16(bytes, 20)? as usize;
    let alias_length = read_u32(bytes, 22)? as usize;

    let mut offset = 26;
    let glyph_bytes = take_section(bytes, &mut offset, glyph_length)?;
    let name_bytes = take_section(bytes, &mut offset, name_length)?;
    let group_bytes = take_section(bytes, &mut offset, group_length)?;
    let flag_bytes = take_section(bytes, &mut offset, flag_length)?;
    let tone_bytes = take_section(bytes, &mut offset, tone_length)?;
    let alias_bytes = take_section(bytes, &mut offset, alias_length)?;
    if offset != bytes.len() {
        return Err("invalid emoji database length".into());
    }
    if group_bytes.len() != record_count.div_ceil(2) {
        return Err("invalid emoji group data length".into());
    }
    if flag_bytes.len() != (26 * 26usize).div_ceil(8) {
        return Err("invalid emoji flag data length".into());
    }
    validate_tones(tone_bytes, record_count)?;

    let mut glyphs = ByteCursor::new(glyph_bytes);
    let mut names = ByteCursor::new(name_bytes);
    let mut emoji = Vec::with_capacity(record_count + flag_count);
    let mut previous = 0u32;

    for id in 0..record_count {
        let delta = unzigzag(glyphs.read_var()?);
        let first = i64::from(previous)
            .checked_add(delta)
            .and_then(|point| u32::try_from(point).ok())
            .ok_or_else(|| "invalid emoji codepoint delta".to_string())?;
        previous = first;
        let length = glyphs.read_byte()? as usize;
        if length == 0 {
            return Err("empty emoji sequence".into());
        }
        let mut points = Vec::with_capacity(length);
        points.push(first);
        while points.len() < length {
            let byte = glyphs.peek_byte()?;
            let point = if byte > 0 && usize::from(byte) <= POINT_TOKENS.len() {
                glyphs.read_byte()?;
                POINT_TOKENS[usize::from(byte) - 1]
            } else {
                glyphs.read_var()?
            };
            points.push(point);
        }
        let unicode = points
            .into_iter()
            .map(|point| {
                char::from_u32(point).ok_or_else(|| "invalid emoji Unicode scalar".to_string())
            })
            .collect::<Result<String, _>>()?;
        let group_byte = group_bytes[id >> 1];
        let nibble = if id & 1 == 0 {
            group_byte & 0x0f
        } else {
            group_byte >> 4
        };
        let group = *EMOJI_GROUPS
            .get(usize::from(nibble))
            .ok_or_else(|| "invalid emoji group".to_string())?;
        let shortcode = names.read_name()?;
        if shortcode.is_empty() {
            return Err("empty emoji shortcode".into());
        }
        let label = shortcode.replace('_', " ");
        emoji.push(EmojiRecord {
            id,
            unicode,
            shortcode,
            label: label.clone(),
            group,
            search: label,
        });
    }
    if !glyphs.is_empty() || !names.is_empty() {
        return Err("invalid emoji record data".into());
    }

    let mut generated_flags = 0;
    for index in 0..26 * 26 {
        if flag_bytes[index >> 3] & (1 << (index & 7)) == 0 {
            continue;
        }
        let first = index / 26;
        let second = index % 26;
        let region = format!(
            "{}{}",
            char::from(b'A' + first as u8),
            char::from(b'A' + second as u8)
        );
        let shortcode = format!("flag_{}", region.to_ascii_lowercase());
        let unicode = [
            char::from_u32(0x1f1e6 + first as u32),
            char::from_u32(0x1f1e6 + second as u32),
        ]
        .into_iter()
        .collect::<Option<String>>()
        .ok_or_else(|| "invalid generated flag".to_string())?;
        let id = emoji.len();
        let label = shortcode.replace('_', " ");
        emoji.push(EmojiRecord {
            id,
            unicode,
            shortcode,
            label: label.clone(),
            group: 9,
            search: label,
        });
        generated_flags += 1;
    }
    if generated_flags != flag_count {
        return Err("invalid generated flag count".into());
    }

    let mut by_shortcode = HashMap::with_capacity(emoji.len());
    for record in &emoji {
        if by_shortcode
            .insert(record.shortcode.clone(), record.id)
            .is_some()
        {
            return Err("duplicate emoji shortcode".into());
        }
    }

    let mut aliases = ByteCursor::new(alias_bytes);
    while !aliases.is_empty() {
        let id = aliases.read_var()? as usize;
        let alias = aliases.read_name()?;
        let record = emoji
            .get_mut(id)
            .ok_or_else(|| "invalid emoji alias record".to_string())?;
        if alias.is_empty() || by_shortcode.contains_key(&alias) {
            return Err("invalid emoji alias".into());
        }
        record.search.push(' ');
        record.search.push_str(&alias.replace('_', " "));
        by_shortcode.insert(alias, id);
    }

    let by_unicode = emoji.iter().map(|record| record.unicode.clone()).collect();
    Ok(EmojiDatabase {
        emoji,
        by_shortcode,
        by_unicode,
    })
}

fn validate_tones(bytes: &[u8], record_count: usize) -> Result<(), String> {
    let mut cursor = ByteCursor::new(bytes);
    let mut record = 0usize;
    while !cursor.is_empty() {
        record = record
            .checked_add(cursor.read_var()? as usize)
            .ok_or_else(|| "emoji tone record overflow".to_string())?;
        if record >= record_count {
            return Err("invalid emoji tone record".into());
        }
        let count = cursor.read_byte()? as usize;
        cursor.take(count)?;
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated emoji database header".to_string())?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated emoji database header".to_string())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn take_section<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
    length: usize,
) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "emoji database section overflow".to_string())?;
    let section = bytes
        .get(*offset..end)
        .ok_or_else(|| "truncated emoji database section".to_string())?;
    *offset = end;
    Ok(section)
}

fn unzigzag(value: u32) -> i64 {
    i64::from(value >> 1) ^ -i64::from(value & 1)
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn peek_byte(&self) -> Result<u8, String> {
        self.bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| "truncated emoji data".to_string())
    }

    fn read_byte(&mut self) -> Result<u8, String> {
        let byte = self.peek_byte()?;
        self.offset += 1;
        Ok(byte)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "emoji data length overflow".to_string())?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "truncated emoji data".to_string())?;
        self.offset = end;
        Ok(value)
    }

    fn read_var(&mut self) -> Result<u32, String> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            if shift > 28 {
                return Err("invalid emoji varuint".into());
            }
            let byte = self.read_byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if value > u64::from(u32::MAX) {
                return Err("emoji varuint overflow".into());
            }
            if byte & 0x80 == 0 {
                return Ok(value as u32);
            }
            shift += 7;
        }
    }

    fn read_name(&mut self) -> Result<String, String> {
        let mut value = String::new();
        loop {
            let byte = self.read_byte()?;
            if byte == 0 {
                return Ok(value);
            }
            if usize::from(byte) <= NAME_TOKENS.len() {
                value.push_str(NAME_TOKENS[usize::from(byte) - 1]);
            } else {
                value.push(char::from(byte));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_bundled_web_database() {
        let database = decode_database(DATABASE_BYTES).unwrap();

        assert_eq!(database.emoji.len(), 1_914);
        assert_eq!(database.by_shortcode.len(), 2_282);
        let smile = &database.emoji[database.by_shortcode["smile"]];
        assert_eq!(smile.unicode, "😄");
        assert_eq!(smile.shortcode, "smile");
        let alias = &database.emoji[database.by_shortcode["grinning_face"]];
        assert_eq!(alias.shortcode, "grinning");
        let flag = &database.emoji[database.by_shortcode["flag_us"]];
        assert_eq!(flag.unicode, "🇺🇸");
    }

    #[test]
    fn rejects_corrupt_or_truncated_databases() {
        assert!(decode_database(b"TE02").is_err());
        let mut corrupt = DATABASE_BYTES.to_vec();
        corrupt[0] = b'X';
        assert!(decode_database(&corrupt).is_err());
        assert!(decode_database(&DATABASE_BYTES[..DATABASE_BYTES.len() - 1]).is_err());
    }

    #[test]
    fn searches_with_web_ranking_and_a_bounded_result() {
        let matches = suggestions("sm", 8);
        assert_eq!(matches.len(), 8);
        assert_eq!(matches[0].shortcode, "smile");
        assert_eq!(matches[1].shortcode, "smirk");
        assert_eq!(suggestions("grinning_face", 8)[0].shortcode, "grinning");
        assert!(
            suggestions("", 8)
                .into_iter()
                .all(|record| record.group == 0)
        );
    }

    #[test]
    fn counts_complete_emoji_graphemes_and_ignores_spacing() {
        assert_eq!(emoji_only_count("😀 ❤️  🇺🇸"), Some(3));
        assert_eq!(emoji_only_count(" \t"), None);
        assert_eq!(emoji_only_count("😀 hello"), None);
        assert_eq!(emoji_only_count("☺"), None);
    }

    #[test]
    fn finds_colon_triggers_and_completed_shortcodes() {
        assert_eq!(
            find_colon_trigger(":sm", 3..3),
            Some(ShortcodeRange {
                range: 0..3,
                shortcode: "sm",
            })
        );
        assert_eq!(
            find_completed_shortcode("hi :smile:", 10),
            Some(ShortcodeRange {
                range: 3..10,
                shortcode: "smile",
            })
        );
        assert!(find_colon_trigger("word:sm", 7..7).is_none());
        assert!(find_colon_trigger("::sm", 4..4).is_none());
        assert!(find_colon_trigger(":sm", 1..3).is_none());
        assert!(find_completed_shortcode(":smile:", 99).is_none());
    }

    #[test]
    fn ignores_inline_and_fenced_markdown_code() {
        assert!(find_colon_trigger("`code :sm", 9..9).is_none());
        assert!(find_completed_shortcode("`code :smile:", 13).is_none());
        assert_eq!(
            find_colon_trigger("`code` :sm", 10..10).unwrap().shortcode,
            "sm"
        );

        let open = "before\n```ts\nconst face = :sm";
        assert!(find_colon_trigger(open, open.len()..open.len()).is_none());
        let closed = "```\n:smile:\n```\nafter :sm";
        assert!(find_completed_shortcode(closed, 11).is_none());
        assert_eq!(
            find_colon_trigger(closed, closed.len()..closed.len())
                .unwrap()
                .shortcode,
            "sm"
        );
    }

    #[test]
    fn escaped_backticks_and_utf8_boundaries_remain_total() {
        let escaped = "\\`literal :sm";
        assert_eq!(
            find_colon_trigger(escaped, escaped.len()..escaped.len())
                .unwrap()
                .shortcode,
            "sm"
        );
        assert!(find_colon_trigger("😀:sm", 1..1).is_none());
        assert!(find_colon_trigger(":é", 3..3).is_some());
        assert!(find_colon_trigger(&format!(":{}", "a".repeat(65)), 66..66).is_none());
    }
}
