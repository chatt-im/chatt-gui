use std::{mem, ops::Range};

use chatt_message_format::{
    Token, TokenKind,
    highlight::{self, HlClass, PaletteRole},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComposerTypeface {
    Ui,
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComposerColor {
    Default,
    Dim,
    Link,
    Syntax(PaletteRole),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ComposerTextStyle {
    pub typeface: ComposerTypeface,
    pub color: ComposerColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub code_background: bool,
}

impl Default for ComposerTextStyle {
    fn default() -> Self {
        Self {
            typeface: ComposerTypeface::Ui,
            color: ComposerColor::Default,
            bold: false,
            italic: false,
            underline: false,
            code_background: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ComposerStyleRun {
    pub range: Range<usize>,
    pub style: ComposerTextStyle,
}

#[derive(Clone, Debug)]
struct CachedCodeBlock {
    tag: Option<String>,
    body: String,
    runs: Vec<(u32, u32, HlClass)>,
}

#[derive(Clone, Debug)]
struct CodeLine {
    source: Range<usize>,
    logical: Range<usize>,
}

#[derive(Default)]
pub(super) struct ComposerSyntax {
    version: Option<u64>,
    runs: Vec<ComposerStyleRun>,
    code_blocks: Vec<CachedCodeBlock>,
    #[cfg(test)]
    refresh_count: usize,
    #[cfg(test)]
    code_highlight_count: usize,
}

impl ComposerSyntax {
    pub fn refresh(&mut self, version: u64, source: &str) {
        if self.version == Some(version) {
            return;
        }
        self.version = Some(version);
        #[cfg(test)]
        {
            self.refresh_count += 1;
        }

        let mut tokens = Vec::new();
        chatt_message_format::tokenize(source, &mut tokens);
        let mut styles = vec![ComposerTextStyle::default(); source.len()];

        apply_structural_styles(source, &tokens, &mut styles);
        self.apply_code_blocks(source, &tokens, &mut styles);
        apply_marker_styles(source, &tokens, &mut styles);
        apply_quote_marker_styles(source, &mut styles);

        self.runs = merge_styles(styles);
    }

    pub fn runs_for(&self, range: Range<usize>) -> impl Iterator<Item = ComposerStyleRun> + '_ {
        let range_start = range.start;
        let range_end = range.end;
        let first = self
            .runs
            .partition_point(|run| run.range.end <= range_start);
        self.runs[first..]
            .iter()
            .take_while(move |run| run.range.start < range_end)
            .filter_map(move |run| {
                let start = run.range.start.max(range_start);
                let end = run.range.end.min(range_end);
                (start < end).then_some(ComposerStyleRun {
                    range: start..end,
                    style: run.style,
                })
            })
    }

    fn apply_code_blocks(
        &mut self,
        source: &str,
        tokens: &[Token],
        styles: &mut [ComposerTextStyle],
    ) {
        let mut old_blocks = mem::take(&mut self.code_blocks);
        let mut new_blocks = Vec::new();
        let mut index = 0;

        while index < tokens.len() {
            let TokenKind::CodeBlockStart { lang } = &tokens[index].kind else {
                index += 1;
                continue;
            };

            let opener = byte_range(&tokens[index].range);
            set_code_range(styles, opener.clone(), ComposerColor::Dim);
            let tag = lang
                .as_ref()
                .map(byte_range)
                .and_then(|range| source.get(range))
                .map(str::to_owned);

            index += 1;
            let mut body = String::new();
            let mut lines = Vec::new();
            let mut first = true;
            while tokens
                .get(index)
                .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockLine))
            {
                let source_range = byte_range(&tokens[index].range);
                if !first {
                    body.push('\n');
                }
                first = false;
                let logical_start = body.len();
                if let Some(line) = source.get(source_range.clone()) {
                    body.push_str(line);
                }
                let logical_end = body.len();
                set_code_range(
                    styles,
                    source_range.clone(),
                    ComposerColor::Syntax(PaletteRole::Foreground),
                );
                lines.push(CodeLine {
                    source: source_range,
                    logical: logical_start..logical_end,
                });
                index += 1;
            }

            let runs = if let Some(position) = old_blocks
                .iter()
                .position(|cached| cached.tag == tag && cached.body == body)
            {
                old_blocks.swap_remove(position).runs
            } else {
                #[cfg(test)]
                {
                    self.code_highlight_count += 1;
                }
                let language = tag.as_deref().and_then(highlight::language_for_tag);
                highlight::source_runs(&(&*body), language)
            };

            for &(run_start, run_end, class) in &runs {
                let run = run_start as usize..run_end as usize;
                for line in &lines {
                    let start = run.start.max(line.logical.start);
                    let end = run.end.min(line.logical.end);
                    if start >= end {
                        continue;
                    }
                    let source_start = line.source.start + start - line.logical.start;
                    let source_end = line.source.start + end - line.logical.start;
                    apply_syntax_range(styles, source_start..source_end, class);
                }
            }

            new_blocks.push(CachedCodeBlock { tag, body, runs });

            if let Some(closer) = tokens
                .get(index)
                .filter(|token| matches!(token.kind, TokenKind::CodeBlockEnd))
            {
                set_code_range(styles, byte_range(&closer.range), ComposerColor::Dim);
                index += 1;
            }
        }

        self.code_blocks = new_blocks;
    }

    #[cfg(test)]
    fn style_at(&self, offset: usize) -> ComposerTextStyle {
        self.runs
            .iter()
            .find(|run| run.range.contains(&offset))
            .map_or_else(ComposerTextStyle::default, |run| run.style)
    }
}

fn apply_structural_styles(source: &str, tokens: &[Token], styles: &mut [ComposerTextStyle]) {
    let mut headers = Vec::new();
    let mut bold = Vec::new();
    let mut italic = Vec::new();

    for token in tokens {
        let range = byte_range(&token.range);
        match &token.kind {
            TokenKind::HeaderStart => headers.push(range.end),
            TokenKind::HeaderEnd => {
                if let Some(start) = headers.pop() {
                    apply_range(styles, start..range.start, |style| {
                        style.bold = true;
                        style.color = ComposerColor::Syntax(PaletteRole::Function);
                    });
                }
            }
            TokenKind::BoldStart => bold.push(range.end),
            TokenKind::BoldEnd => {
                if let Some(start) = bold.pop() {
                    apply_range(styles, start..range.start, |style| style.bold = true);
                }
            }
            TokenKind::ItalicStart => italic.push(range.end),
            TokenKind::ItalicEnd => {
                if let Some(start) = italic.pop() {
                    apply_range(styles, start..range.start, |style| style.italic = true);
                }
            }
            TokenKind::InlineCode => {
                apply_range(styles, range.clone(), |style| {
                    style.typeface = ComposerTypeface::Code;
                    style.color = ComposerColor::Syntax(PaletteRole::String);
                    style.code_background = true;
                });
                if range.start > 0 && source.as_bytes().get(range.start - 1) == Some(&b'`') {
                    apply_range(styles, range.start - 1..range.start, inline_code_marker);
                }
                if source.as_bytes().get(range.end) == Some(&b'`') {
                    apply_range(styles, range.end..range.end + 1, inline_code_marker);
                }
            }
            TokenKind::Url => apply_range(styles, range, |style| {
                style.color = ComposerColor::Link;
                style.underline = true;
            }),
            TokenKind::MessageRef => apply_range(styles, range, |style| {
                style.typeface = ComposerTypeface::Code;
                style.color = ComposerColor::Dim;
            }),
            _ => {}
        }
    }
}

fn apply_marker_styles(source: &str, tokens: &[Token], styles: &mut [ComposerTextStyle]) {
    for token in tokens {
        let range = byte_range(&token.range);
        match &token.kind {
            TokenKind::HeaderStart
            | TokenKind::BoldStart
            | TokenKind::BoldEnd
            | TokenKind::ItalicStart
            | TokenKind::ItalicEnd => apply_range(styles, range, dim_marker),
            TokenKind::ListItemStart { marker } => {
                apply_range(styles, byte_range(marker), dim_marker);
            }
            TokenKind::CodeBlockStart { .. } | TokenKind::CodeBlockEnd => {
                set_code_range(styles, range, ComposerColor::Dim);
            }
            TokenKind::InlineCode => {
                if range.start > 0 && source.as_bytes().get(range.start - 1) == Some(&b'`') {
                    apply_range(styles, range.start - 1..range.start, inline_code_marker);
                }
                if source.as_bytes().get(range.end) == Some(&b'`') {
                    apply_range(styles, range.end..range.end + 1, inline_code_marker);
                }
            }
            _ => {}
        }
    }
}

fn apply_quote_marker_styles(source: &str, styles: &mut [ComposerTextStyle]) {
    let bytes = source.as_bytes();
    let mut line_start = 0;
    while line_start < bytes.len() {
        let mut cursor = line_start;
        while bytes.get(cursor) == Some(&b'>') {
            cursor += 1;
            if bytes.get(cursor) == Some(&b' ') {
                cursor += 1;
            }
        }
        apply_range(styles, line_start..cursor, dim_marker);
        line_start = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| line_start + offset + 1);
    }
}

fn inline_code_marker(style: &mut ComposerTextStyle) {
    style.typeface = ComposerTypeface::Code;
    style.color = ComposerColor::Dim;
    style.code_background = true;
}

fn dim_marker(style: &mut ComposerTextStyle) {
    style.color = ComposerColor::Dim;
}

fn set_code_range(styles: &mut [ComposerTextStyle], range: Range<usize>, color: ComposerColor) {
    apply_range(styles, range, |style| {
        style.typeface = ComposerTypeface::Code;
        style.color = color;
        style.bold = false;
        style.italic = false;
        style.underline = false;
        style.code_background = false;
    });
}

fn apply_syntax_range(styles: &mut [ComposerTextStyle], range: Range<usize>, class: HlClass) {
    apply_range(styles, range, |style| {
        style.typeface = ComposerTypeface::Code;
        style.color = ComposerColor::Syntax(class.palette_role());
        style.italic = matches!(class, HlClass::Comment | HlClass::DocComment);
    });
}

fn apply_range(
    styles: &mut [ComposerTextStyle],
    range: Range<usize>,
    mut apply: impl FnMut(&mut ComposerTextStyle),
) {
    let start = range.start.min(styles.len());
    let end = range.end.min(styles.len()).max(start);
    for style in &mut styles[start..end] {
        apply(style);
    }
}

fn merge_styles(styles: Vec<ComposerTextStyle>) -> Vec<ComposerStyleRun> {
    let Some(&first) = styles.first() else {
        return Vec::new();
    };
    let mut runs = Vec::new();
    let mut start = 0;
    let mut current = first;
    for (index, style) in styles.iter().copied().enumerate().skip(1) {
        if style != current {
            runs.push(ComposerStyleRun {
                range: start..index,
                style: current,
            });
            start = index;
            current = style;
        }
    }
    runs.push(ComposerStyleRun {
        range: start..styles.len(),
        style: current,
    });
    runs
}

fn byte_range(range: &Range<u32>) -> Range<usize> {
    range.start as usize..range.end as usize
}

#[cfg(test)]
mod tests {
    use super::{ComposerColor, ComposerSyntax, ComposerTypeface, PaletteRole};

    #[test]
    fn styles_chatt_message_syntax_without_enabling_markdown_only_forms() {
        let source = "# Title\n- **bold** and *italic* plus _plain_ `code` https://example.com @@1c9k3m5n7p2tq\n> quote";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let title = source.find("Title").unwrap();
        assert!(syntax.style_at(title).bold);
        assert_eq!(
            syntax.style_at(title).color,
            ComposerColor::Syntax(PaletteRole::Function)
        );

        let bold = source.find("bold").unwrap();
        assert!(syntax.style_at(bold).bold);
        let italic = source.find("italic").unwrap();
        assert!(syntax.style_at(italic).italic);
        let markdown_only = source.find("_plain_").unwrap();
        assert_eq!(syntax.style_at(markdown_only), Default::default());

        let code = source.find("code").unwrap();
        assert_eq!(syntax.style_at(code).typeface, ComposerTypeface::Code);
        assert!(syntax.style_at(code).code_background);
        let url = source.find("https://").unwrap();
        assert!(syntax.style_at(url).underline);
        let reference = source.find("@@1c").unwrap();
        assert_eq!(syntax.style_at(reference).typeface, ComposerTypeface::Code);
        let quote = source.rfind('>').unwrap();
        assert_eq!(syntax.style_at(quote).color, ComposerColor::Dim);
    }

    #[test]
    fn tagged_fence_is_monospace_and_uses_tinyhl_colors() {
        let source = "before\n```rust\nfn main() {\n    // note\n}\n```\nafter";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let prose = source.find("before").unwrap();
        assert_eq!(syntax.style_at(prose).typeface, ComposerTypeface::Ui);
        let opener = source.find("```rust").unwrap();
        assert_eq!(syntax.style_at(opener).typeface, ComposerTypeface::Code);
        assert_eq!(syntax.style_at(opener).color, ComposerColor::Dim);
        let keyword = source.find("fn main").unwrap();
        assert_eq!(
            syntax.style_at(keyword).color,
            ComposerColor::Syntax(PaletteRole::Keyword)
        );
        let comment = source.find("// note").unwrap();
        assert_eq!(
            syntax.style_at(comment).color,
            ComposerColor::Syntax(PaletteRole::Comment)
        );
        assert!(syntax.style_at(comment).italic);
        let closer = source.rfind("```").unwrap();
        assert_eq!(syntax.style_at(closer).typeface, ComposerTypeface::Code);
        assert_eq!(syntax.style_at(closer).color, ComposerColor::Dim);
    }

    #[test]
    fn quoted_code_maps_logical_highlights_back_to_source() {
        let source = "> ```rust\n> fn main() {}\n> ```";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let quote = source.find('>').unwrap();
        assert_eq!(syntax.style_at(quote).typeface, ComposerTypeface::Ui);
        assert_eq!(syntax.style_at(quote).color, ComposerColor::Dim);
        let keyword = source.find("fn main").unwrap();
        assert_eq!(syntax.style_at(keyword).typeface, ComposerTypeface::Code);
        assert_eq!(
            syntax.style_at(keyword).color,
            ComposerColor::Syntax(PaletteRole::Keyword)
        );
    }

    #[test]
    fn unknown_fence_stays_plain_but_entirely_monospace() {
        let source = "```madeup\nfn main\n```";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let body = source.find("fn main").unwrap();
        assert_eq!(syntax.style_at(body).typeface, ComposerTypeface::Code);
        assert_eq!(
            syntax.style_at(body).color,
            ComposerColor::Syntax(PaletteRole::Foreground)
        );
    }

    #[test]
    fn chatt_language_aliases_drive_code_highlighting() {
        let source = "```javascript\nconst answer = 42;\n```";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let keyword = source.find("const").unwrap();
        assert_eq!(
            syntax.style_at(keyword).color,
            ComposerColor::Syntax(PaletteRole::Keyword)
        );
    }

    #[test]
    fn unclosed_fence_remains_plain_like_the_chatt_message_format() {
        let source = "```rust\nfn main() {}";
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, source);

        let fence = source.find("```").unwrap();
        let body = source.find("fn main").unwrap();
        assert_eq!(syntax.style_at(fence), Default::default());
        assert_eq!(syntax.style_at(body), Default::default());
    }

    #[test]
    fn refresh_reuses_unchanged_code_block_highlights() {
        let mut syntax = ComposerSyntax::default();
        syntax.refresh(1, "one\n```rust\nfn one() {}\n```");
        assert_eq!(syntax.refresh_count, 1);
        assert_eq!(syntax.code_highlight_count, 1);

        syntax.refresh(1, "ignored because the version did not change");
        assert_eq!(syntax.refresh_count, 1);
        assert_eq!(syntax.code_highlight_count, 1);

        syntax.refresh(2, "two\n```rust\nfn one() {}\n```");
        assert_eq!(syntax.refresh_count, 2);
        assert_eq!(syntax.code_highlight_count, 1);

        syntax.refresh(3, "two\n```rust\nfn two() {}\n```");
        assert_eq!(syntax.code_highlight_count, 2);
    }
}
