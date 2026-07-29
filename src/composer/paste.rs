use std::borrow::Cow;

use chatt_message_format::{is_fence_line, quote_prefix};

/// Rewrites a paste so it lands inside the Markdown block at `offset`.
///
/// Fence and quote recognition comes from the same grammar that renders the
/// sent message. Line endings are normalized because platform clipboards may
/// contain CRLF or lone CR even though the composer stores LF.
pub(super) fn markdown_paste_insertion(source: &str, offset: usize, paste: &str) -> String {
    let offset = offset.min(source.len());
    let paste = normalize_line_endings(paste);
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    let line = &source[line_start..line_end];
    let prefix = quote_prefix(line);

    let mut paste = paste.as_ref();
    let mut insertion = String::with_capacity(paste.len() + prefix.len() + 1);
    if offset == line_end && is_fence_line(&line[prefix.len()..]) {
        if !paste.starts_with('\n') {
            insertion.push('\n');
        }
        paste = paste.strip_suffix('\n').unwrap_or(paste);
    }
    insertion.push_str(paste);

    if prefix.is_empty() || offset < line_start + prefix.len() {
        return insertion;
    }
    continue_quote(&insertion, prefix)
}

fn continue_quote(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len() + prefix.len());
    let mut lines = text.split('\n').peekable();
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    while let Some(line) = lines.next() {
        out.push('\n');
        if line.is_empty() && lines.peek().is_some() {
            out.push_str(prefix.trim_end());
        } else {
            out.push_str(prefix);
        }
        out.push_str(line);
    }
    out
}

fn normalize_line_endings(paste: &str) -> Cow<'_, str> {
    if !paste.contains('\r') {
        return Cow::Borrowed(paste);
    }
    let mut out = String::with_capacity(paste.len());
    let mut rest = paste;
    while let Some(index) = rest.find('\r') {
        out.push_str(&rest[..index]);
        out.push('\n');
        rest = &rest[index + 1..];
        if let Some(tail) = rest.strip_prefix('\n') {
            rest = tail;
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::markdown_paste_insertion;

    fn paste_at(source: &str, paste: &str) -> String {
        let offset = source.find('|').expect("cursor marker");
        let source = source.replace('|', "");
        let insertion = markdown_paste_insertion(&source, offset, paste);
        format!("{}{insertion}{}", &source[..offset], &source[offset..])
    }

    #[test]
    fn paste_on_fence_line_starts_a_new_line() {
        assert_eq!(paste_at("```|\n```", "code"), "```\ncode\n```");
    }

    #[test]
    fn paste_on_fence_line_with_language_starts_a_new_line() {
        assert_eq!(
            paste_at("```rust|\n```", "fn main() {}"),
            "```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn paste_on_fence_after_prose_starts_a_new_line() {
        assert_eq!(
            paste_at("Hello ```rust|\n```", "code"),
            "Hello ```rust\ncode\n```"
        );
    }

    #[test]
    fn paste_at_closing_fence_end_starts_a_new_line() {
        assert_eq!(
            paste_at("```\ncode\n```|", "after"),
            "```\ncode\n```\nafter"
        );
    }

    #[test]
    fn paste_before_fence_info_string_stays_inline() {
        assert_eq!(paste_at("```|rust\n```", "sh"), "```shrust\n```");
    }

    #[test]
    fn paste_inside_fence_body_is_unchanged() {
        assert_eq!(paste_at("```\n|\n```", "code"), "```\ncode\n```");
    }

    #[test]
    fn paste_into_fence_drops_one_trailing_newline() {
        assert_eq!(paste_at("```|\n```", "code\n"), "```\ncode\n```");
    }

    #[test]
    fn paste_starting_with_a_newline_is_not_doubled() {
        assert_eq!(paste_at("```|\n```", "\ncode"), "```\ncode\n```");
    }

    #[test]
    fn paste_on_quote_line_quotes_every_added_line() {
        assert_eq!(paste_at("> |", "first\nsecond"), "> first\n> second");
    }

    #[test]
    fn paste_keeps_nested_quote_prefix() {
        assert_eq!(paste_at(">> |", "first\nsecond"), ">> first\n>> second");
    }

    #[test]
    fn paste_before_quote_marker_is_unchanged() {
        assert_eq!(
            paste_at("|> quoted", "first\nsecond"),
            "first\nsecond> quoted"
        );
    }

    #[test]
    fn paste_blank_lines_keep_the_bare_quote_marker() {
        assert_eq!(paste_at("> |", "first\n\nsecond"), "> first\n>\n> second");
    }

    #[test]
    fn paste_ending_in_newline_keeps_the_quote_marker_for_typing() {
        assert_eq!(paste_at("> |", "first\n"), "> first\n> ");
    }

    #[test]
    fn paste_on_quoted_fence_line_quotes_the_inserted_break() {
        assert_eq!(paste_at("> ```|\n> ```", "code"), "> ```\n> code\n> ```");
    }

    #[test]
    fn paste_normalizes_crlf_line_endings() {
        assert_eq!(
            paste_at("|", "first\r\nsecond\rthird"),
            "first\nsecond\nthird"
        );
    }

    #[test]
    fn paste_outside_markdown_blocks_is_verbatim() {
        assert_eq!(paste_at("hello |world", "big "), "hello big world");
    }
}
