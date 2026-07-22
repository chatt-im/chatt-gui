use std::{cell::RefCell, collections::BTreeMap, ops::Range, rc::Rc, sync::Arc};

use chatt_message_format::{
    Token, TokenKind,
    highlight::{self, HlClass, PaletteRole},
};
use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Edges,
    Element, ElementId, FocusHandle, FontStyle, FontWeight, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, KeyBinding, KeyContext, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, SharedString, StyledText, TextLayout, TextRun,
    TextStyle, UnderlineStyle, WhiteSpace, Window, actions, div, point, prelude::*, px, quad, rgb,
    rgba,
};

use crate::{
    fonts::{CODE_FONT_FAMILY, UI_FONT_FAMILY},
    icons::{IconName, icon},
};

const BODY_COLOR: u32 = 0xd8d8d8;
const DIM_COLOR: u32 = 0x8a8a8a;
const LINK_COLOR: u32 = 0xf0f0f0;
const QUOTE_RAIL_COLOR: u32 = 0x4a4a4a;
const CODE_BACKGROUND: u32 = 0x0e0e0e;
const CODE_BORDER: u32 = 0x2a2a2a;
const MAX_VISIBLE_QUOTE_DEPTH: usize = 8;

actions!(formatted_message, [Copy]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-c", Copy, Some("ChattFormattedText"))]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineKind {
    Plain,
    Url,
    Reference,
    Code,
    ListMarker,
    Syntax(HlClass),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FormatSpan {
    start: usize,
    end: usize,
    kind: InlineKind,
    bold: bool,
    italic: bool,
}

#[derive(Clone, Debug)]
struct TextPiece {
    range: Range<usize>,
    text: SharedString,
    spans: Box<[FormatSpan]>,
    cached_runs: RefCell<Option<(TextStyle, Arc<[TextRun]>)>>,
}

#[derive(Debug)]
enum PreparedBlockKind {
    Paragraph(TextPiece),
    Header(TextPiece),
    ListItem {
        marker: TextPiece,
        content: TextPiece,
    },
    Code {
        text: TextPiece,
    },
    Blank,
}

#[derive(Debug)]
struct PreparedBlock {
    quote_depth: usize,
    kind: PreparedBlockKind,
}

#[derive(Debug)]
enum BlockKind {
    Paragraph(TextPiece),
    Header(TextPiece),
    ListItem {
        marker: TextPiece,
        content: TextPiece,
    },
    Code {
        text: TextPiece,
        scroll_handle: ScrollHandle,
        hover_group: SharedString,
    },
    Blank,
}

#[derive(Debug)]
struct Block {
    quote_depth: usize,
    kind: BlockKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderedLink {
    range: Range<usize>,
    destination: SharedString,
}

#[derive(Default)]
struct InteractionState {
    pressed_link: Option<RenderedLink>,
}

/// Parsed and highlighted presentation data cached for one timeline message.
pub struct FormattedMessage {
    source: SharedString,
    visible: SharedString,
    blocks: Vec<Block>,
    links: Rc<[RenderedLink]>,
    interaction: RefCell<InteractionState>,
}

/// Renderer-independent work that can be completed off the GPUI thread.
pub(crate) struct PreparedFormattedMessage {
    source: SharedString,
    visible: SharedString,
    blocks: Vec<PreparedBlock>,
    links: Vec<RenderedLink>,
}

impl FormattedMessage {
    #[cfg(test)]
    pub fn new(source: impl Into<SharedString>) -> Self {
        Self::from_prepared(Self::prepare(source))
    }

    pub(crate) fn prepare(source: impl Into<SharedString>) -> PreparedFormattedMessage {
        let source = source.into();
        let mut tokens = Vec::new();
        chatt_message_format::tokenize(&source, &mut tokens);
        let (visible, blocks, links) = project_document(&source, &tokens);
        PreparedFormattedMessage {
            source,
            visible: visible.into(),
            blocks,
            links,
        }
    }

    pub(crate) fn from_prepared(prepared: PreparedFormattedMessage) -> Self {
        let blocks = prepared
            .blocks
            .into_iter()
            .map(|block| Block {
                quote_depth: block.quote_depth,
                kind: match block.kind {
                    PreparedBlockKind::Paragraph(piece) => BlockKind::Paragraph(piece),
                    PreparedBlockKind::Header(piece) => BlockKind::Header(piece),
                    PreparedBlockKind::ListItem { marker, content } => {
                        BlockKind::ListItem { marker, content }
                    }
                    PreparedBlockKind::Code { text } => BlockKind::Code {
                        hover_group: format!("formatted-code-{}", text.range.start).into(),
                        text,
                        scroll_handle: ScrollHandle::new(),
                    },
                    PreparedBlockKind::Blank => BlockKind::Blank,
                },
            })
            .collect();
        Self {
            source: prepared.source,
            visible: prepared.visible,
            blocks,
            links: prepared.links.into(),
            interaction: RefCell::new(InteractionState::default()),
        }
    }

    pub(crate) fn plain(source: impl Into<SharedString>) -> Self {
        let source = source.into();
        let text = source.clone();
        let len = text.len();
        Self {
            source,
            visible: text.clone(),
            blocks: vec![Block {
                quote_depth: 0,
                kind: BlockKind::Paragraph(TextPiece {
                    range: 0..len,
                    text,
                    spans: vec![FormatSpan {
                        start: 0,
                        end: len,
                        kind: InlineKind::Plain,
                        bold: false,
                        italic: false,
                    }]
                    .into_boxed_slice(),
                    cached_runs: RefCell::new(None),
                }),
            }],
            links: Rc::from([]),
            interaction: RefCell::new(InteractionState::default()),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

impl PreparedFormattedMessage {
    pub(crate) fn source(&self) -> &str {
        &self.source
    }
}

fn project_document(
    source: &str,
    tokens: &[Token],
) -> (String, Vec<PreparedBlock>, Vec<RenderedLink>) {
    let mut visible = String::new();
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    let mut quote_depth = 0usize;
    let mut cursor = 0usize;

    while cursor < tokens.len() {
        match &tokens[cursor].kind {
            TokenKind::BlockQuoteStart => {
                quote_depth = quote_depth.saturating_add(1);
                cursor += 1;
            }
            TokenKind::BlockQuoteEnd => {
                quote_depth = quote_depth.saturating_sub(1);
                cursor += 1;
            }
            TokenKind::ParagraphStart => {
                let end = find_token(tokens, cursor + 1, |kind| {
                    matches!(kind, TokenKind::ParagraphEnd)
                });
                let projected = project_inline(source, &tokens[cursor + 1..end]);
                let piece = append_piece(&mut visible, projected, &mut links, true);
                blocks.push(PreparedBlock {
                    quote_depth,
                    kind: PreparedBlockKind::Paragraph(piece),
                });
                cursor = end.saturating_add(1);
            }
            TokenKind::HeaderStart => {
                let end = find_token(tokens, cursor + 1, |kind| {
                    matches!(kind, TokenKind::HeaderEnd)
                });
                let projected = project_inline(source, &tokens[cursor + 1..end]);
                let piece = append_piece(&mut visible, projected, &mut links, true);
                blocks.push(PreparedBlock {
                    quote_depth,
                    kind: PreparedBlockKind::Header(piece),
                });
                cursor = end.saturating_add(1);
            }
            TokenKind::ListItemStart { marker } => {
                let end = find_token(tokens, cursor + 1, |kind| {
                    matches!(kind, TokenKind::ListItemEnd)
                });
                append_block_separator(&mut visible);
                let marker_text = &source[marker.start as usize..marker.end as usize];
                let marker_start = visible.len();
                visible.push_str(marker_text);
                let marker_piece = TextPiece {
                    range: marker_start..visible.len(),
                    text: marker_text.into(),
                    spans: vec![FormatSpan {
                        start: 0,
                        end: marker_text.len(),
                        kind: InlineKind::ListMarker,
                        bold: true,
                        italic: false,
                    }]
                    .into_boxed_slice(),
                    cached_runs: RefCell::new(None),
                };
                let projected = project_inline(source, &tokens[cursor + 1..end]);
                let content_piece = append_piece(&mut visible, projected, &mut links, false);
                blocks.push(PreparedBlock {
                    quote_depth,
                    kind: PreparedBlockKind::ListItem {
                        marker: marker_piece,
                        content: content_piece,
                    },
                });
                cursor = end.saturating_add(1);
            }
            TokenKind::CodeBlockStart { lang } => {
                let tag = lang
                    .as_ref()
                    .map(|range| &source[range.start as usize..range.end as usize]);
                cursor += 1;
                let mut code = String::new();
                let mut first = true;
                while tokens
                    .get(cursor)
                    .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockLine))
                {
                    if !first {
                        code.push('\n');
                    }
                    first = false;
                    let range = &tokens[cursor].range;
                    code.push_str(&source[range.start as usize..range.end as usize]);
                    cursor += 1;
                }
                let language = tag.and_then(highlight::language_for_tag);
                let spans = highlight::source_runs(&(&*code), language)
                    .into_iter()
                    .map(|(start, end, class)| FormatSpan {
                        start: start as usize,
                        end: end as usize,
                        kind: InlineKind::Syntax(class),
                        bold: false,
                        italic: false,
                    })
                    .collect();
                let projected = ProjectedInline {
                    text: code,
                    spans,
                    links: Vec::new(),
                };
                let piece = append_piece(&mut visible, projected, &mut links, true);
                blocks.push(PreparedBlock {
                    quote_depth,
                    kind: PreparedBlockKind::Code { text: piece },
                });
                if tokens
                    .get(cursor)
                    .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockEnd))
                {
                    cursor += 1;
                }
            }
            TokenKind::BlankLine => {
                append_block_separator(&mut visible);
                visible.push('\n');
                blocks.push(PreparedBlock {
                    quote_depth,
                    kind: PreparedBlockKind::Blank,
                });
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }

    (visible, blocks, links)
}

fn find_token(tokens: &[Token], start: usize, predicate: impl Fn(&TokenKind) -> bool) -> usize {
    tokens[start..]
        .iter()
        .position(|token| predicate(&token.kind))
        .map_or(tokens.len(), |offset| start + offset)
}

fn append_block_separator(visible: &mut String) {
    if !visible.is_empty() && !visible.ends_with('\n') {
        visible.push('\n');
    }
}

#[derive(Default)]
struct ProjectedInline {
    text: String,
    spans: Vec<FormatSpan>,
    links: Vec<(Range<usize>, SharedString)>,
}

fn project_inline(source: &str, tokens: &[Token]) -> ProjectedInline {
    let mut projected = ProjectedInline::default();
    let mut bold = false;
    let mut italic = false;

    for token in tokens {
        match &token.kind {
            TokenKind::BoldStart => bold = true,
            TokenKind::BoldEnd => bold = false,
            TokenKind::ItalicStart => italic = true,
            TokenKind::ItalicEnd => italic = false,
            TokenKind::Text | TokenKind::Url | TokenKind::MessageRef | TokenKind::InlineCode => {
                let text = &source[token.range.start as usize..token.range.end as usize];
                let start = projected.text.len();
                projected.text.push_str(text);
                let end = projected.text.len();
                let kind = match token.kind {
                    TokenKind::Url => InlineKind::Url,
                    TokenKind::MessageRef => InlineKind::Reference,
                    TokenKind::InlineCode => InlineKind::Code,
                    _ => InlineKind::Plain,
                };
                projected.spans.push(FormatSpan {
                    start,
                    end,
                    kind,
                    bold,
                    italic,
                });
                if matches!(token.kind, TokenKind::Url) {
                    projected.links.push((start..end, text.into()));
                }
            }
            TokenKind::HardBreak => {
                let start = projected.text.len();
                projected.text.push('\n');
                projected.spans.push(FormatSpan {
                    start,
                    end: start + 1,
                    kind: InlineKind::Plain,
                    bold,
                    italic,
                });
            }
            _ => {}
        }
    }

    projected
}

fn append_piece(
    visible: &mut String,
    projected: ProjectedInline,
    links: &mut Vec<RenderedLink>,
    separated: bool,
) -> TextPiece {
    if separated {
        append_block_separator(visible);
    }
    let start = visible.len();
    visible.push_str(&projected.text);
    links.extend(
        projected
            .links
            .into_iter()
            .map(|(range, destination)| RenderedLink {
                range: start + range.start..start + range.end,
                destination,
            }),
    );
    TextPiece {
        range: start..visible.len(),
        text: projected.text.into(),
        spans: projected.spans.into_boxed_slice(),
        cached_runs: RefCell::new(None),
    }
}

#[derive(Clone, Copy)]
enum PiecePresentation {
    Body,
    Header,
    Code,
}

pub struct RenderedMessage {
    element: AnyElement,
    text: RenderedText,
}

/// GPUI element that renders one cached [`FormattedMessage`].
pub struct FormattedMessageElement {
    message: Rc<FormattedMessage>,
    selection: Option<(MessageSelectionGroup, MessageSelectionKey)>,
}

impl FormattedMessageElement {
    pub fn new(message: Rc<FormattedMessage>) -> Self {
        Self {
            message,
            selection: None,
        }
    }

    pub fn selection_group(
        mut self,
        group: MessageSelectionGroup,
        key: MessageSelectionKey,
    ) -> Self {
        self.selection = Some((group, key));
        self
    }

    fn build(&self, window: &mut Window, _cx: &mut App) -> RenderedMessage {
        let mut lines = Vec::new();
        let mut root = div().w_full().min_w_0().flex().flex_col();

        for (index, block) in self.message.blocks.iter().enumerate() {
            let element = match &block.kind {
                BlockKind::Paragraph(piece) => render_piece(
                    piece,
                    PiecePresentation::Body,
                    block.quote_depth,
                    true,
                    &mut lines,
                    window,
                ),
                BlockKind::Header(piece) => div()
                    .w_full()
                    .min_w_0()
                    .text_size(px(17.))
                    .child(render_piece(
                        piece,
                        PiecePresentation::Header,
                        block.quote_depth,
                        true,
                        &mut lines,
                        window,
                    ))
                    .into_any_element(),
                BlockKind::ListItem { marker, content } => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .child(render_piece(
                        marker,
                        PiecePresentation::Body,
                        block.quote_depth,
                        false,
                        &mut lines,
                        window,
                    ))
                    .child(div().flex_1().w_0().child(render_piece(
                        content,
                        PiecePresentation::Body,
                        block.quote_depth,
                        true,
                        &mut lines,
                        window,
                    )))
                    .into_any_element(),
                BlockKind::Code {
                    text,
                    scroll_handle,
                    hover_group,
                } => render_code_block(
                    text,
                    scroll_handle,
                    hover_group,
                    block.quote_depth,
                    &mut lines,
                    window,
                ),
                BlockKind::Blank => div().h(px(7.)).into_any_element(),
            };

            let element = div()
                .w_full()
                .min_w_0()
                .when(
                    index > 0 && !matches!(block.kind, BlockKind::Blank),
                    |this| this.pt(px(5.)),
                )
                .child(element)
                .into_any_element();
            root = root.child(wrap_quote(element, block.quote_depth));
        }

        let text = RenderedText {
            visible: self.message.visible.clone(),
            lines: lines.into(),
            links: self.message.links.clone(),
        };
        RenderedMessage {
            element: root.into_any_element(),
            text,
        }
    }

    fn paint_mouse_listeners(
        &self,
        hitbox: &Hitbox,
        rendered: &RenderedText,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let hovering_link = hitbox.is_hovered(window)
            && !self
                .selection
                .as_ref()
                .is_some_and(|(group, _)| group.is_pending())
            && rendered
                .index_for_position(window.mouse_position())
                .ok()
                .is_some_and(|index| rendered.link_at(index).is_some());
        window.set_cursor_style(
            if hovering_link {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            },
            hitbox,
        );

        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let rendered = rendered.clone();
            let selection = self.selection.clone();
            let message = self.message.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let position = rendered.index_for_position(event.position);
                if let Ok(index) = position
                    && let Some(link) = rendered.link_at(index)
                {
                    message.interaction.borrow_mut().pressed_link = Some(link.clone());
                    window.prevent_default();
                    return;
                }
                message.interaction.borrow_mut().pressed_link = None;
                if let Some((group, key)) = selection.as_ref() {
                    let index = match position {
                        Ok(index) | Err(index) => index,
                    };
                    group.begin_selection(
                        *key,
                        index,
                        event.click_count,
                        event.modifiers.shift,
                        &rendered,
                    );
                    window.focus(&group.focus_handle(), cx);
                    window.prevent_default();
                    cx.refresh_windows();
                }
            }
        });

        window.on_mouse_event({
            let rendered = rendered.clone();
            let message = self.message.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                let Some(pressed) = message.interaction.borrow_mut().pressed_link.take() else {
                    return;
                };
                let released = rendered
                    .index_for_position(event.position)
                    .ok()
                    .and_then(|index| rendered.link_at(index));
                if released == Some(&pressed) {
                    cx.open_url(&pressed.destination);
                }
            }
        });
    }
}

impl Element for FormattedMessageElement {
    type RequestLayoutState = RenderedMessage;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut rendered = self.build(window, cx);
        let child = rendered.element.request_layout(window, cx);
        let layout = window.request_layout(gpui::Style::default(), [child], cx);
        (layout, rendered)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        rendered.element.prepaint(window, _cx);
        if let Some((group, key)) = self.selection.as_ref() {
            group.register(*key, bounds, rendered.text.clone());
        }
        hitbox
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        rendered: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut context = KeyContext::default();
        context.add("ChattFormattedText");
        window.set_key_context(context);
        self.paint_mouse_listeners(hitbox, &rendered.text, window, cx);
        rendered.element.paint(window, cx);
        if let Some((group, key)) = self.selection.as_ref()
            && let Some(range) = group.selected_range(*key)
        {
            for bounds in rendered.text.bounds_for_range(range) {
                window.paint_quad(quad(
                    bounds,
                    Pixels::ZERO,
                    rgba(0x5277a866),
                    Edges::default(),
                    Hsla::transparent_black(),
                    BorderStyle::default(),
                ));
            }
        }
    }
}

impl IntoElement for FormattedMessageElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn render_piece(
    piece: &TextPiece,
    presentation: PiecePresentation,
    quote_depth: usize,
    fill_width: bool,
    lines: &mut Vec<RenderedLine>,
    window: &Window,
) -> AnyElement {
    let text = piece.text.clone();
    let runs = text_runs(piece, presentation, quote_depth, window);
    let styled = StyledText::new(text).with_shared_runs(runs);
    lines.push(RenderedLine {
        layout: styled.layout().clone(),
        range: piece.range.clone(),
    });
    div()
        .when(fill_width, |element| element.w_full())
        .min_w_0()
        .child(styled)
        .into_any_element()
}

fn render_code_block(
    piece: &TextPiece,
    scroll_handle: &ScrollHandle,
    hover_group: &SharedString,
    quote_depth: usize,
    lines: &mut Vec<RenderedLine>,
    window: &Window,
) -> AnyElement {
    let code = piece.text.clone();
    let content = if code.is_empty() {
        div().h(px(20.)).into_any_element()
    } else {
        let runs = text_runs(piece, PiecePresentation::Code, quote_depth, window);
        let styled = StyledText::new(code.clone()).with_shared_runs(runs);
        lines.push(RenderedLine {
            layout: styled.layout().clone(),
            range: piece.range.clone(),
        });
        div()
            .id(("formatted-code-scroll", piece.range.start))
            .flex()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(scroll_handle)
            .whitespace_nowrap()
            .child(styled)
            .into_any_element()
    };

    let copy_code = code.clone();
    div()
        .group(hover_group.clone())
        .relative()
        .w_full()
        .min_w_0()
        .px(px(8.))
        .py(px(8.))
        .pr(px(40.))
        .bg(rgb(CODE_BACKGROUND))
        .border_1()
        .border_color(rgb(CODE_BORDER))
        .text_size(px(14.))
        .font_family(CODE_FONT_FAMILY)
        .child(content)
        .child(
            div()
                .id(("copy-code", piece.range.start))
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x1d1d1d))
                .text_color(rgb(DIM_COLOR))
                .cursor_pointer()
                .invisible()
                .group_hover(hover_group.clone(), |button| button.visible())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_code.to_string()));
                })
                .child(icon(IconName::Copy, 15., DIM_COLOR)),
        )
        .into_any_element()
}

fn wrap_quote(mut element: AnyElement, depth: usize) -> AnyElement {
    for _ in 0..depth.min(MAX_VISIBLE_QUOTE_DEPTH) {
        element = div()
            .w_full()
            .min_w_0()
            .pl(px(10.))
            .border_l_4()
            .border_color(rgb(QUOTE_RAIL_COLOR))
            .child(element)
            .into_any_element();
    }
    element
}

fn text_runs(
    piece: &TextPiece,
    presentation: PiecePresentation,
    quote_depth: usize,
    window: &Window,
) -> Arc<[TextRun]> {
    let base_style = base_text_style(presentation, quote_depth, window);
    if let Some((cached_style, cached_runs)) = piece.cached_runs.borrow().as_ref()
        && cached_style == &base_style
    {
        return cached_runs.clone();
    }
    let text = &piece.text;
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    for span in &piece.spans {
        if span.start > cursor {
            runs.push(base_style.to_run(span.start - cursor));
        }
        if span.end <= span.start || span.end > text.len() {
            continue;
        }
        let mut style = base_style.clone();
        apply_span_style(&mut style, *span);
        runs.push(style.to_run(span.end - span.start));
        cursor = span.end;
    }
    if cursor < text.len() {
        runs.push(base_style.to_run(text.len() - cursor));
    }
    let runs: Arc<[TextRun]> = runs.into();
    piece.cached_runs.replace(Some((base_style, runs.clone())));
    runs
}

fn base_text_style(
    presentation: PiecePresentation,
    quote_depth: usize,
    window: &Window,
) -> TextStyle {
    let mut style = window.text_style();
    style.color = rgb(if quote_depth > 0 {
        DIM_COLOR
    } else {
        BODY_COLOR
    })
    .into();
    style.font_family = match presentation {
        PiecePresentation::Code => CODE_FONT_FAMILY.into(),
        PiecePresentation::Body | PiecePresentation::Header => UI_FONT_FAMILY.into(),
    };
    style.font_weight = if matches!(presentation, PiecePresentation::Header) {
        FontWeight::SEMIBOLD
    } else {
        FontWeight::NORMAL
    };
    if matches!(presentation, PiecePresentation::Code) {
        style.white_space = WhiteSpace::Nowrap;
        style.color = rgb(syntax_color(PaletteRole::Foreground)).into();
    }
    style
}

fn apply_span_style(style: &mut TextStyle, span: FormatSpan) {
    if span.bold {
        style.font_weight = FontWeight::BOLD;
    }
    if span.italic {
        style.font_style = FontStyle::Italic;
    }
    match span.kind {
        InlineKind::Plain => {}
        InlineKind::Url => {
            style.color = rgb(LINK_COLOR).into();
            style.underline = Some(UnderlineStyle {
                color: Some(rgb(LINK_COLOR).into()),
                thickness: px(1.),
                ..Default::default()
            });
        }
        InlineKind::Reference => {
            style.color = rgb(DIM_COLOR).into();
            style.font_family = CODE_FONT_FAMILY.into();
        }
        InlineKind::Code => {
            style.font_family = CODE_FONT_FAMILY.into();
            style.background_color = Some(rgba(0xffffff14).into());
        }
        InlineKind::ListMarker => {
            style.font_weight = FontWeight::BOLD;
        }
        InlineKind::Syntax(class) => {
            style.color = rgb(syntax_color(class.palette_role())).into();
            if matches!(class, HlClass::Comment | HlClass::DocComment) {
                style.font_style = FontStyle::Italic;
            }
        }
    }
}

const fn syntax_color(role: PaletteRole) -> u32 {
    match role {
        PaletteRole::Foreground => 0xbdc0be,
        PaletteRole::Type => 0xebc782,
        PaletteRole::Function => 0x8aa6bd,
        PaletteRole::Binding => 0xc87270,
        PaletteRole::Namespace => 0xd99a6d,
        PaletteRole::Keyword => 0xb49bbb,
        PaletteRole::String => 0xb8be77,
        PaletteRole::Number => 0xcccccc,
        PaletteRole::Comment => 0x8a8c8a,
    }
}

#[derive(Clone)]
struct RenderedLine {
    layout: TextLayout,
    range: Range<usize>,
}

#[derive(Clone)]
struct RenderedText {
    visible: SharedString,
    lines: Rc<[RenderedLine]>,
    links: Rc<[RenderedLink]>,
}

impl RenderedText {
    fn source_end(&self) -> usize {
        self.visible.len()
    }

    fn line_height(&self) -> Option<Pixels> {
        self.lines.first().map(|line| line.layout.line_height())
    }

    fn index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let line = self
            .lines
            .iter()
            .find(|line| line.layout.bounds().contains(&position))
            .or_else(|| {
                self.lines.iter().min_by_key(|line| {
                    let bounds = line.layout.bounds();
                    let dy = if position.y < bounds.top() {
                        bounds.top() - position.y
                    } else if position.y > bounds.bottom() {
                        position.y - bounds.bottom()
                    } else {
                        Pixels::ZERO
                    };
                    let dx = if position.x < bounds.left() {
                        bounds.left() - position.x
                    } else if position.x > bounds.right() {
                        position.x - bounds.right()
                    } else {
                        Pixels::ZERO
                    };
                    (dy, dx)
                })
            });
        let Some(line) = line else {
            return Err(0);
        };
        match line.layout.index_for_position(position) {
            Ok(index) => Ok((line.range.start + index).min(line.range.end)),
            Err(index) => Err((line.range.start + index).min(line.range.end)),
        }
    }

    fn link_at(&self, index: usize) -> Option<&RenderedLink> {
        self.links.iter().find(|link| link.range.contains(&index))
    }

    fn surrounding_word_range(&self, index: usize) -> Range<usize> {
        if self.visible.is_empty() {
            return 0..0;
        }
        let mut index = index.min(self.visible.len());
        while index > 0 && !self.visible.is_char_boundary(index) {
            index -= 1;
        }
        if index == self.visible.len() {
            index = self.visible[..index]
                .char_indices()
                .next_back()
                .map_or(0, |(offset, _)| offset);
        }
        let Some(current) = self.visible[index..].chars().next() else {
            return index..index;
        };
        let class = word_class(current);
        let mut start = index;
        for (offset, character) in self.visible[..index].char_indices().rev() {
            if word_class(character) != class {
                break;
            }
            start = offset;
        }
        let mut end = index + current.len_utf8();
        for character in self.visible[end..].chars() {
            if word_class(character) != class {
                break;
            }
            end += character.len_utf8();
        }
        start..end
    }

    fn surrounding_line_range(&self, index: usize) -> Range<usize> {
        let index = index.min(self.visible.len());
        let start = self.visible[..index]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        let end = self.visible[index..]
            .find('\n')
            .map_or(self.visible.len(), |offset| index + offset);
        start..end
    }

    fn push_text_for_range(&self, range: Range<usize>, output: &mut String) {
        let mut start = range.start.min(self.visible.len());
        let mut end = range.end.min(self.visible.len());
        while start > 0 && !self.visible.is_char_boundary(start) {
            start -= 1;
        }
        while end < self.visible.len() && !self.visible.is_char_boundary(end) {
            end += 1;
        }
        output.push_str(&self.visible[start..end]);
    }

    fn bounds_for_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut result: Vec<Bounds<Pixels>> = Vec::new();
        for line in self.lines.iter() {
            let start = range.start.max(line.range.start);
            let end = range.end.min(line.range.end);
            if start >= end {
                continue;
            }
            let local_start = start - line.range.start;
            let local_end = end - line.range.start;
            let layout_bounds = line.layout.bounds();
            let line_height = line.layout.line_height();
            let mut physical_start = 0usize;
            let mut row_top = layout_bounds.top();

            for physical in line.layout.line_layouts() {
                let physical_end = physical_start + physical.len();
                let selected_start = local_start.max(physical_start);
                let selected_end = local_end.min(physical_end);
                let unwrapped = &physical.unwrapped_layout;
                let row_ends = physical
                    .wrap_boundaries()
                    .iter()
                    .map(|boundary| {
                        let glyph = &unwrapped.runs[boundary.run_ix].glyphs[boundary.glyph_ix];
                        (physical_start + glyph.index, glyph.position.x)
                    })
                    .chain([(physical_end, unwrapped.width)]);
                let mut row_start = physical_start;
                let mut row_start_x = Pixels::ZERO;

                for (row_end, row_end_x) in row_ends {
                    let row_selection_start = selected_start.max(row_start);
                    let row_selection_end = selected_end.min(row_end);
                    if row_selection_start < row_selection_end {
                        let start_x = unwrapped.x_for_index(row_selection_start - physical_start)
                            - row_start_x;
                        let end_x =
                            unwrapped.x_for_index(row_selection_end - physical_start) - row_start_x;
                        result.push(Bounds::from_corners(
                            point(layout_bounds.left() + start_x, row_top),
                            point(
                                layout_bounds.left() + end_x.max(start_x + px(1.)),
                                row_top + line_height,
                            ),
                        ));
                    }
                    row_start = row_end;
                    row_start_x = row_end_x;
                    row_top += line_height;
                }
                physical_start = physical_end.saturating_add(1);
            }
        }
        result
    }
}

fn word_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MessageSelectionKey(pub u64);

#[derive(Clone)]
pub struct MessageSelectionGroup(Rc<RefCell<MessageSelectionGroupState>>);

#[derive(Clone)]
struct MessageSelectionParticipant {
    bounds: Bounds<Pixels>,
    text: RenderedText,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MessageSelectionEndpoint {
    key: MessageSelectionKey,
    offset: usize,
}

#[derive(Clone)]
enum SelectMode {
    Character,
    Word,
    Line,
    All,
}

#[derive(Clone)]
struct ActiveMessageSelection {
    anchor_key: MessageSelectionKey,
    anchor_range: Range<usize>,
    head_key: MessageSelectionKey,
    head_offset: usize,
    start: MessageSelectionEndpoint,
    end: MessageSelectionEndpoint,
    reversed: bool,
    pending: bool,
    mode: SelectMode,
}

struct MessageSelectionGroupState {
    focus_handle: FocusHandle,
    current: Vec<(MessageSelectionKey, MessageSelectionParticipant)>,
    retained: BTreeMap<MessageSelectionKey, MessageSelectionParticipant>,
    active: Option<ActiveMessageSelection>,
}

impl MessageSelectionGroup {
    pub fn new(focus_handle: FocusHandle) -> Self {
        Self(Rc::new(RefCell::new(MessageSelectionGroupState {
            focus_handle,
            current: Vec::new(),
            retained: BTreeMap::new(),
            active: None,
        })))
    }

    pub fn clear(&self) {
        let state = &mut *self.0.borrow_mut();
        state.active = None;
        state.current.clear();
        state.retained.clear();
    }

    pub fn retain_items(&self, keys: impl IntoIterator<Item = MessageSelectionKey>) {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        let state = &mut *self.0.borrow_mut();
        if state.active.as_ref().is_some_and(|active| {
            keys.binary_search(&active.anchor_key).is_err()
                || keys.binary_search(&active.head_key).is_err()
        }) {
            state.active = None;
            state.retained.clear();
        } else {
            state
                .retained
                .retain(|key, _| keys.binary_search(key).is_ok());
        }
        state
            .current
            .retain(|(key, _)| keys.binary_search(key).is_ok());
    }

    fn focus_handle(&self) -> FocusHandle {
        self.0.borrow().focus_handle.clone()
    }

    fn begin_frame(&self) {
        self.0.borrow_mut().current.clear();
    }

    fn register(&self, key: MessageSelectionKey, bounds: Bounds<Pixels>, text: RenderedText) {
        let state = &mut *self.0.borrow_mut();
        let participant = MessageSelectionParticipant { bounds, text };
        state.current.push((key, participant.clone()));
        if state.active.as_ref().is_some_and(|active| {
            active.pending || (key >= active.start.key && key <= active.end.key)
        }) {
            state.retained.insert(key, participant);
        }
    }

    fn contains_position(&self, position: Point<Pixels>) -> bool {
        self.0
            .borrow()
            .current
            .iter()
            .any(|(_, participant)| participant.bounds.contains(&position))
    }

    fn begin_selection(
        &self,
        key: MessageSelectionKey,
        offset: usize,
        click_count: usize,
        extend: bool,
        text: &RenderedText,
    ) {
        let state = &mut *self.0.borrow_mut();
        let extending = extend && click_count == 1 && state.active.is_some();
        let (anchor_key, anchor_range, mode) = if extending {
            let active = state.active.as_ref().unwrap();
            let tail = if active.reversed {
                active.end
            } else {
                active.start
            };
            (tail.key, tail.offset..tail.offset, SelectMode::Character)
        } else {
            match click_count {
                1 => (key, offset..offset, SelectMode::Character),
                2 => {
                    let range = text.surrounding_word_range(offset);
                    (key, range, SelectMode::Word)
                }
                3 => {
                    let range = text.surrounding_line_range(offset);
                    (key, range, SelectMode::Line)
                }
                _ => (key, 0..text.source_end(), SelectMode::All),
            }
        };
        let current = state.current.clone();
        if extending {
            state.retained.extend(current);
        } else {
            state.retained = current.into_iter().collect();
        }
        state.active = Some(ActiveMessageSelection {
            anchor_key,
            anchor_range: anchor_range.clone(),
            head_key: key,
            head_offset: offset,
            start: MessageSelectionEndpoint {
                key: anchor_key,
                offset: anchor_range.start,
            },
            end: MessageSelectionEndpoint {
                key: anchor_key,
                offset: anchor_range.end,
            },
            reversed: false,
            pending: true,
            mode,
        });
        state.recompute_active_selection();
    }

    fn update_head_for_position(&self, position: Point<Pixels>) -> bool {
        let Some((key, participant)) = self.0.borrow().participant_nearest(position) else {
            return false;
        };
        let offset = match participant.text.index_for_position(position) {
            Ok(offset) | Err(offset) => offset,
        };
        self.update_head(key, offset)
    }

    fn update_head(&self, key: MessageSelectionKey, offset: usize) -> bool {
        let state = &mut *self.0.borrow_mut();
        if !state.active.as_ref().is_some_and(|active| active.pending)
            || !state.retained.contains_key(&key)
        {
            return false;
        }
        let active = state.active.as_mut().unwrap();
        if active.head_key == key && active.head_offset == offset {
            return false;
        }
        active.head_key = key;
        active.head_offset = offset;
        state.recompute_active_selection();
        true
    }

    fn finish_selection(&self) -> Option<String> {
        let state = &mut *self.0.borrow_mut();
        let Some(active) = state.active.as_mut() else {
            return None;
        };
        if !active.pending {
            return None;
        }
        active.pending = false;
        if active.start >= active.end {
            let anchor_key = active.anchor_key;
            state.retained.retain(|key, _| *key == anchor_key);
            return None;
        }
        let start_key = active.start.key;
        let end_key = active.end.key;
        state
            .retained
            .retain(|key, _| *key >= start_key && *key <= end_key);
        state.selected_text()
    }

    fn is_pending(&self) -> bool {
        self.0
            .borrow()
            .active
            .as_ref()
            .is_some_and(|active| active.pending)
    }

    fn is_active(&self) -> bool {
        self.0.borrow().active.is_some()
    }

    fn active_line_height(&self) -> Option<Pixels> {
        let state = self.0.borrow();
        let active = state.active.as_ref()?;
        state
            .current
            .iter()
            .find(|(key, _)| *key == active.head_key)
            .map(|(_, participant)| participant)
            .and_then(|participant| participant.text.line_height())
    }

    fn selected_range(&self, key: MessageSelectionKey) -> Option<Range<usize>> {
        self.0.borrow().selected_range(key)
    }

    fn selected_text(&self) -> Option<String> {
        self.0.borrow().selected_text()
    }
}

impl MessageSelectionGroupState {
    fn participant_nearest(
        &self,
        position: Point<Pixels>,
    ) -> Option<(MessageSelectionKey, MessageSelectionParticipant)> {
        self.current
            .iter()
            .min_by_key(|(_, participant)| {
                if position.y < participant.bounds.top() {
                    participant.bounds.top() - position.y
                } else if position.y > participant.bounds.bottom() {
                    position.y - participant.bounds.bottom()
                } else {
                    Pixels::ZERO
                }
            })
            .map(|(key, participant)| (*key, participant.clone()))
    }

    fn recompute_active_selection(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(head) = self.retained.get(&active.head_key) else {
            return;
        };
        let anchor_end = self
            .retained
            .get(&active.anchor_key)
            .map_or(0, |participant| participant.text.source_end());
        let anchor_start = MessageSelectionEndpoint {
            key: active.anchor_key,
            offset: active.anchor_range.start,
        };
        let anchor_end_point = MessageSelectionEndpoint {
            key: active.anchor_key,
            offset: active.anchor_range.end,
        };
        let head_point = MessageSelectionEndpoint {
            key: active.head_key,
            offset: active.head_offset,
        };

        match active.mode {
            SelectMode::Character => {
                if head_point < anchor_start {
                    active.start = head_point;
                    active.end = anchor_start;
                    active.reversed = true;
                } else {
                    active.start = anchor_start;
                    active.end = head_point;
                    active.reversed = false;
                }
            }
            SelectMode::Word | SelectMode::Line => {
                let head_range = if matches!(active.mode, SelectMode::Word) {
                    head.text.surrounding_word_range(active.head_offset)
                } else {
                    head.text.surrounding_line_range(active.head_offset)
                };
                if head_point < anchor_start {
                    active.start = MessageSelectionEndpoint {
                        key: active.head_key,
                        offset: head_range.start,
                    };
                    active.end = anchor_end_point;
                    active.reversed = true;
                } else if head_point >= anchor_end_point {
                    active.start = anchor_start;
                    active.end = MessageSelectionEndpoint {
                        key: active.head_key,
                        offset: head_range.end,
                    };
                    active.reversed = false;
                } else {
                    active.start = anchor_start;
                    active.end = anchor_end_point;
                    active.reversed = false;
                }
            }
            SelectMode::All => {
                active.start = MessageSelectionEndpoint {
                    key: active.anchor_key,
                    offset: 0,
                };
                active.end = MessageSelectionEndpoint {
                    key: active.anchor_key,
                    offset: anchor_end,
                };
                active.reversed = false;
            }
        }
    }

    fn selected_range(&self, key: MessageSelectionKey) -> Option<Range<usize>> {
        let Some(active) = self.active.as_ref() else {
            return None;
        };
        if key < active.start.key || key > active.end.key {
            return None;
        }
        let end = self.retained.get(&key)?.text.source_end();
        Some(if active.start.key == active.end.key {
            active.start.offset.min(end)..active.end.offset.min(end)
        } else if key == active.start.key {
            active.start.offset.min(end)..end
        } else if key == active.end.key {
            0..active.end.offset.min(end)
        } else {
            0..end
        })
    }

    fn selected_text(&self) -> Option<String> {
        let active = self.active.as_ref()?;
        if active.start >= active.end {
            return None;
        }
        let mut selected = String::new();
        let mut first = true;
        for (key, participant) in self.retained.range(active.start.key..=active.end.key) {
            let Some(range) = self.selected_range(*key) else {
                continue;
            };
            if !first {
                selected.push_str("\n\n");
            }
            first = false;
            participant.text.push_text_for_range(range, &mut selected);
        }
        Some(selected)
    }
}

type SelectionAutoscrollHandler = Box<dyn FnMut(Pixels, &mut Window, &mut App) + 'static>;

/// Owns pointer and copy handling for all formatted messages in the timeline.
pub struct MessageSelectionArea<E> {
    inner: E,
    group: MessageSelectionGroup,
    on_vertical_autoscroll: Option<SelectionAutoscrollHandler>,
}

impl<E> MessageSelectionArea<E> {
    pub fn new(inner: E, group: MessageSelectionGroup) -> Self {
        Self {
            inner,
            group,
            on_vertical_autoscroll: None,
        }
    }

    pub fn on_vertical_autoscroll(
        mut self,
        handler: impl FnMut(Pixels, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_vertical_autoscroll = Some(Box::new(handler));
        self
    }
}

impl<E> Element for MessageSelectionArea<E>
where
    E: Element + IntoElement<Element = E>,
{
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.inner.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.inner.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        self.inner.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.group.begin_frame();
        window.set_focus_handle(&self.group.focus_handle(), cx);
        let prepaint = self
            .inner
            .prepaint(id, inspector_id, bounds, request, window, cx);
        if self.group.update_head_for_position(window.mouse_position()) {
            cx.refresh_windows();
        }
        if self.group.is_pending() {
            let pointer = window.mouse_position();
            let line_height = self.group.active_line_height().unwrap_or(px(16.));
            let margin = line_height.min(bounds.size.height / 3.);
            let delta = if pointer.y < bounds.top() + margin {
                -selection_autoscroll_delta(bounds.top() + margin - pointer.y, line_height)
            } else if pointer.y > bounds.bottom() - margin {
                selection_autoscroll_delta(pointer.y - (bounds.bottom() - margin), line_height)
            } else {
                Pixels::ZERO
            };
            if delta != Pixels::ZERO
                && let Some(handler) = self.on_vertical_autoscroll.as_mut()
            {
                handler(delta, window, cx);
                window.request_animation_frame();
            }
        }
        prepaint
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut context = KeyContext::default();
        context.add("ChattFormattedText");
        window.set_key_context(context);
        window.on_action(std::any::TypeId::of::<Copy>(), {
            let group = self.group.clone();
            move |_, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(text) = group.selected_text()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    cx.stop_propagation();
                }
            }
        });
        window.on_mouse_event({
            let group = self.group.clone();
            move |event: &MouseDownEvent, phase, _window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && !group.contains_position(event.position)
                    && group.is_active()
                {
                    group.clear();
                    cx.refresh_windows();
                }
            }
        });
        window.on_mouse_event({
            let group = self.group.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase.bubble() && group.update_head_for_position(event.position) {
                    cx.refresh_windows();
                }
            }
        });
        window.on_mouse_event({
            let group = self.group.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && let Some(text) = group.finish_selection()
                {
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    cx.write_to_primary(ClipboardItem::new_string(text));
                    cx.refresh_windows();
                }
            }
        });
        self.inner
            .paint(id, inspector_id, bounds, request, prepaint, window, cx);
    }
}

impl<E> IntoElement for MessageSelectionArea<E>
where
    E: Element + IntoElement<Element = E>,
{
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn selection_autoscroll_delta(distance: Pixels, line_height: Pixels) -> Pixels {
    let lines = f32::from((distance.pow(1.2) / 100.).min(px(3.)));
    line_height * lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(text: &str) -> RenderedText {
        RenderedText {
            visible: text.to_string().into(),
            lines: Vec::new().into(),
            links: Vec::new().into(),
        }
    }

    fn render_message(
        source: &str,
        width: f32,
        cx: &mut gpui::VisualTestContext,
    ) -> (Rc<FormattedMessage>, RenderedMessage) {
        let message = Rc::new(FormattedMessage::new(source));
        let drawn = message.clone();
        let (rendered, _) = cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(width), px(600.)),
            move |_, _| FormattedMessageElement::new(drawn),
        );
        (message, rendered)
    }

    fn visible(source: &str) -> String {
        FormattedMessage::new(source).visible.to_string()
    }

    #[test]
    fn projects_chatt_subset_without_source_delimiters() {
        assert_eq!(
            visible("# Head\n\nplain **bold** and `code`"),
            "Head\n\nplain bold and code"
        );
    }

    #[test]
    fn unsupported_markdown_stays_literal() {
        assert_eq!(
            visible("## nope [label](https://example.test)"),
            "## nope [label](https://example.test)"
        );
    }

    #[test]
    fn reconstructs_fenced_code_without_fences() {
        assert_eq!(
            visible("before\n```rust\nfn main() {}\n```\nafter"),
            "before\nfn main() {}\nafter"
        );
    }

    #[test]
    fn assigns_web_palette_roles_to_syntax_runs() {
        let message = FormattedMessage::new("```rust\nfn main() {}\n```");
        let BlockKind::Code { text, .. } = &message.blocks[0].kind else {
            panic!("expected code block");
        };
        assert!(text.spans.iter().any(|span| {
            matches!(span.kind, InlineKind::Syntax(HlClass::Keyword))
                && syntax_color(HlClass::Keyword.palette_role()) == 0xb49bbb
        }));
    }

    #[gpui::test]
    fn list_marker_keeps_intrinsic_width(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let cx = cx.add_empty_window();
        let (_, rendered) = render_message("- list content with several words", 500., cx);
        assert_eq!(rendered.text.lines.len(), 2);
        let marker = rendered.text.lines[0].layout.bounds();
        let content = rendered.text.lines[1].layout.bounds();

        assert!(marker.size.width < px(80.));
        assert!(content.size.width > px(300.));
    }

    #[gpui::test]
    fn selection_bounds_are_batched_by_wrapped_row(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let cx = cx.add_empty_window();
        let source = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        let (_, rendered) = render_message(source, 140., cx);
        let line = &rendered.text.lines[0];
        let visual_rows = line
            .layout
            .line_layouts()
            .iter()
            .map(|line| line.wrap_boundaries().len() + 1)
            .sum::<usize>();
        let bounds = rendered.text.bounds_for_range(0..source.len());

        assert_eq!(bounds.len(), visual_rows);
        assert!(bounds.iter().all(|bounds| bounds.size.width > Pixels::ZERO));
    }

    #[gpui::test]
    fn text_runs_are_shared_across_layouts(cx: &mut gpui::TestAppContext) {
        cx.update(crate::fonts::init);
        let cx = cx.add_empty_window();
        let (message, _) = render_message("plain **bold** text", 500., cx);
        let BlockKind::Paragraph(piece) = &message.blocks[0].kind else {
            panic!("expected paragraph");
        };
        let first = piece.cached_runs.borrow().as_ref().unwrap().1.clone();

        let drawn = message.clone();
        let _ = cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(500.), px(600.)),
            move |_, _| FormattedMessageElement::new(drawn),
        );
        let second = piece.cached_runs.borrow().as_ref().unwrap().1.clone();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[gpui::test]
    fn empty_selection_retains_only_its_shift_click_anchor(cx: &mut gpui::TestAppContext) {
        let group = MessageSelectionGroup::new(cx.update(|cx| cx.focus_handle()));
        let key = MessageSelectionKey(1);
        let text = rendered_text("alpha");
        group.register(key, Bounds::default(), text.clone());
        group.begin_selection(key, 2, 1, false, &text);

        assert_eq!(group.finish_selection(), None);
        let state = group.0.borrow();
        assert!(state.active.is_some());
        assert_eq!(state.retained.len(), 1);
        assert!(state.retained.contains_key(&key));
        drop(state);

        group.begin_selection(key, 4, 1, true, &text);
        assert_eq!(group.finish_selection().as_deref(), Some("ph"));
    }

    #[gpui::test]
    fn completed_selection_retains_only_its_key_span(cx: &mut gpui::TestAppContext) {
        let group = MessageSelectionGroup::new(cx.update(|cx| cx.focus_handle()));
        let first = rendered_text("alpha");
        let second = rendered_text("second");
        let third = rendered_text("third");
        group.register(MessageSelectionKey(1), Bounds::default(), first.clone());
        group.register(MessageSelectionKey(2), Bounds::default(), second.clone());
        group.register(MessageSelectionKey(3), Bounds::default(), third);
        group.begin_selection(MessageSelectionKey(1), 2, 1, false, &first);
        assert!(group.update_head(MessageSelectionKey(2), 3));

        assert_eq!(group.finish_selection().as_deref(), Some("pha\n\nsec"));
        let state = group.0.borrow();
        assert_eq!(state.retained.len(), 2);
        assert!(!state.retained.contains_key(&MessageSelectionKey(3)));
    }
}
