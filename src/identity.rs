//! The end-to-end identity review dialog.
//!
//! Every value shown here arrives already rendered by the daemon: the key
//! groups, the identity words, the copyable verification text, and the verdict
//! on whatever the user pastes. This view decides layout and focus and nothing
//! else, so the renderer links no wordlist and parses no key material.

use gpui::{
    AnyElement, App, ClipboardItem, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, KeyDownEvent, Render, ScrollHandle, SharedString, StyledText,
    Subscription, div, prelude::*,
};

use local_rpc::identity as wire;

use crate::{
    composer::{ComposerChanged, TextEditor},
    icons::{IconName, icon},
    settings::ConfigurationState,
    theme::{AppliedSettings, ResolvedSettings, ThemePalette, ThemeRole},
    ui_scale::rems_from_px,
};

/// Words per row in the comparison grid. The terminal dialog picks a column
/// count from its character width; six keeps the same shape at default scale
/// and stays readable when the window is narrow.
const WORD_COLUMNS: usize = 6;
const WIDE_DIALOG_WIDTH: f32 = 760.;

pub(crate) enum IdentityViewEvent {
    Closed,
    Command(wire::IdentityCommand),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IdentityTab {
    #[default]
    Peer,
    Local,
}

pub(crate) struct IdentityView {
    document: Option<wire::IdentityDocument>,
    tab: IdentityTab,
    paste: Entity<TextEditor>,
    _paste_subscription: Subscription,
    check: Option<wire::VerificationCheck>,
    /// The user's own attestation that they compared all the words out of band.
    /// Purely local, exactly as in the terminal dialog: it unlocks the button,
    /// and the daemon still re-checks the pin before it commits anything.
    words_confirmed: bool,
    /// Forgetting a verification is destructive, so the button asks twice.
    forget_confirmation: bool,
    status: Option<SharedString>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl IdentityView {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let binding_mode = cx
            .global::<ConfigurationState>()
            .0
            .config
            .input
            .default_binding_mode;
        let paste = cx.new(|cx| {
            TextEditor::settings_input("paste their verification text here", binding_mode, cx)
        });
        let subscription = cx.subscribe(&paste, |this, paste, _: &ComposerChanged, cx| {
            let text = paste.read(cx).text();
            this.check_text(text, cx);
        });
        Self {
            document: None,
            tab: IdentityTab::default(),
            paste,
            _paste_subscription: subscription,
            check: None,
            words_confirmed: false,
            forget_confirmation: false,
            status: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        }
    }

    /// Installs a document pushed by the daemon, reporting whether it describes
    /// an identity the view was not already showing.
    ///
    /// A new revision means the reviewed identity moved, so the local
    /// attestations are dropped rather than carried onto a key the user has not
    /// compared.
    pub(crate) fn apply_document(
        &mut self,
        document: wire::IdentityDocument,
        cx: &mut Context<Self>,
    ) -> bool {
        let fresh = self
            .document
            .as_ref()
            .is_none_or(|current| current.revision != document.revision);
        let superseded = self.document.is_some() && fresh;
        if superseded {
            self.words_confirmed = false;
            self.forget_confirmation = false;
            self.check = None;
            self.paste
                .update(cx, |paste, cx| paste.set_value(String::new(), cx));
        }
        if !document.can_verify {
            self.tab = IdentityTab::Peer;
        }
        self.document = Some(document);
        cx.notify();
        fresh
    }

    pub(crate) fn apply_check(
        &mut self,
        session_id: wire::IdentitySessionId,
        revision: u64,
        check: wire::VerificationCheck,
        cx: &mut Context<Self>,
    ) {
        // A verdict for a superseded document describes a key that is no longer
        // on screen, so it is dropped rather than shown against the new one.
        let current = self.document.as_ref().is_some_and(|document| {
            document.session_id == session_id && document.revision == revision
        });
        if !current {
            return;
        }
        self.check = Some(check);
        cx.notify();
    }

    pub(crate) fn clear_check(&mut self, cx: &mut Context<Self>) {
        self.check = None;
        cx.notify();
    }

    pub(crate) fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(status.into());
        cx.notify();
    }

    pub(crate) fn session_id(&self) -> Option<wire::IdentitySessionId> {
        self.document.as_ref().map(|document| document.session_id)
    }

    /// Sends the field's contents to be checked.
    ///
    /// Blank input is sent too rather than cleared locally: the daemon answers
    /// it with "no verdict", and routing every edit through the same round trip
    /// is what keeps the displayed verdict describing the text on screen. A
    /// local shortcut would let a reply for the text the user just deleted land
    /// afterwards and strand the dialog on a verdict for a field that is empty.
    fn check_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id() else {
            return;
        };
        self.forget_confirmation = false;
        cx.emit(IdentityViewEvent::Command(
            wire::IdentityCommand::CheckText { session_id, text },
        ));
    }

    fn verification_passed(&self) -> bool {
        self.words_confirmed || matches!(self.check, Some(wire::VerificationCheck::Match))
    }

    fn confirm_words(&mut self, cx: &mut Context<Self>) {
        if matches!(self.check, Some(wire::VerificationCheck::Invalid { .. })) {
            return;
        }
        self.words_confirmed = !self.words_confirmed;
        cx.notify();
    }

    fn verify(&mut self, cx: &mut Context<Self>) {
        let Some(document) = &self.document else {
            return;
        };
        if !document.can_verify || !self.verification_passed() {
            return;
        }
        cx.emit(IdentityViewEvent::Command(wire::IdentityCommand::Verify {
            session_id: document.session_id,
            revision: document.revision,
        }));
    }

    fn forget(&mut self, cx: &mut Context<Self>) {
        let Some(document) = &self.document else {
            return;
        };
        if !document.can_forget {
            return;
        }
        if !self.forget_confirmation {
            self.forget_confirmation = true;
            cx.notify();
            return;
        }
        cx.emit(IdentityViewEvent::Command(wire::IdentityCommand::Forget {
            session_id: document.session_id,
            revision: document.revision,
        }));
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(IdentityViewEvent::Closed);
    }

    fn select_tab(&mut self, tab: IdentityTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = Some("Copied to clipboard".into());
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        // Escape belongs to the editor while it is in a non-normal mode, so a
        // vim user leaves insert before the dialog closes under them.
        if key == "escape" && self.paste.read(cx).mode() != crate::composer::Mode::Normal {
            return;
        }
        match key {
            "escape" => self.close(cx),
            "tab" => {
                let tab = match self.tab {
                    IdentityTab::Peer => IdentityTab::Local,
                    IdentityTab::Local => IdentityTab::Peer,
                };
                self.select_tab(tab, cx);
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }
}

impl EventEmitter<IdentityViewEvent> for IdentityView {}

impl Focusable for IdentityView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        let Some(document) = &self.document else {
            return self.focus.clone();
        };
        if document.can_verify {
            self.paste.focus_handle(cx)
        } else {
            self.focus.clone()
        }
    }
}

impl Render for IdentityView {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let applied = AppliedSettings::get(cx);
        let palette = applied.theme.clone();
        let logical_width = f32::from(window.viewport_size().width)
            / f32::from(crate::ui_scale::rem_size(cx))
            * crate::ui_scale::BASE_REM_SIZE;
        let compact = logical_width < WIDE_DIALOG_WIDTH;

        let body = match &self.document {
            Some(document) => {
                self.render_document(document, &palette, &applied, compact, window, cx)
            }
            // Nothing to review yet. The status line goes here too: without a
            // document there is no footer, and a refusal to open the review has
            // nowhere else to be seen.
            None => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(palette.color(ThemeRole::TextMuted))
                .child("Fetching the encryption identity…")
                .when_some(self.status.clone(), |body, status| {
                    body.child(status_line(status, &palette))
                })
                .into_any_element(),
        };
        let title = match &self.document {
            Some(document) => format!("Encryption identity: {}", document.username),
            None => "Encryption identity".to_string(),
        };

        div()
            .id("identity")
            .key_context("ChattIdentity ChattModal")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::handle_key_down))
            .absolute()
            .inset_0()
            .p_6()
            .flex()
            .items_center()
            .justify_center()
            .bg(palette.color(ThemeRole::Scrim))
            .child(
                div()
                    .w_full()
                    .max_w(rems_from_px(WIDE_DIALOG_WIDTH))
                    .max_h_full()
                    .overflow_hidden()
                    .border_1()
                    .border_color(palette.color(ThemeRole::BorderStrong))
                    .bg(palette.color(ThemeRole::Raised))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .font_family(applied.fonts.interface_family.clone())
                    .text_color(palette.color(ThemeRole::TextPrimary))
                    .child(
                        div()
                            .flex_none()
                            .min_h(rems_from_px(56.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_5()
                            .border_b_1()
                            .border_color(palette.color(ThemeRole::BorderSubtle))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child(title),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("identity-close")
                                    .p_1()
                                    .cursor_pointer()
                                    .child(icon(
                                        IconName::Close,
                                        18.,
                                        palette.color(ThemeRole::TextMuted),
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                            ),
                    )
                    .child(body),
            )
    }
}

impl IdentityView {
    fn render_document(
        &self,
        document: &wire::IdentityDocument,
        palette: &ThemePalette,
        applied: &ResolvedSettings,
        compact: bool,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let peer_tab = self.tab == IdentityTab::Peer;
        let shown = if peer_tab {
            &document.peer
        } else {
            &document.local
        };
        let code_family = applied.fonts.code_family.clone();

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("identity-body")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(status_banner(&document.status, palette))
                    .child(paragraph(
                        "An encryption identity is the public key Chatt uses to protect this DM. \
                         Confirming it means you compared it outside this DM.",
                        palette,
                    ))
                    .when_some(document.error.clone(), |column, error| {
                        column.child(
                            div()
                                .text_sm()
                                .text_color(palette.color(ThemeRole::StateDanger))
                                .child(error),
                        )
                    })
                    .child(self.render_tabs(document, palette, cx))
                    .child(section_title("X25519 public key", palette))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .gap_x_3()
                            .font_family(code_family.clone())
                            .text_sm()
                            .text_color(palette.color(ThemeRole::TextSecondary))
                            .children(
                                shown
                                    .key_groups
                                    .iter()
                                    .map(|group| div().child(group.clone())),
                            ),
                    )
                    .child(self.render_words_header(document, peer_tab, palette, cx))
                    .child(word_grid(&shown.words, code_family, compact, palette))
                    .when(peer_tab, |column| {
                        column
                            .child(section_title("How to confirm this identity", palette))
                            .child(paragraph(
                                format!(
                                    "Call {}, meet in person, or use another trusted service. \
                                     Compare every one of the {} identity words. This DM is not an \
                                     independent verification channel.",
                                    document.username,
                                    shown.words.len()
                                ),
                                palette,
                            ))
                    })
                    .when(!peer_tab, |column| {
                        column.child(paragraph(
                            "The other person compares these exact words with the identity they \
                             see for you.",
                            palette,
                        ))
                    })
                    .child(self.render_copy_row(document, palette, cx)),
            )
            .when(document.can_verify, |panel| {
                panel.child(self.render_verification_input(
                    document.peer.words.len(),
                    palette,
                    window,
                    cx,
                ))
            })
            .child(self.render_footer(document, palette, cx))
            .into_any_element()
    }

    fn render_tabs(
        &self,
        document: &wire::IdentityDocument,
        palette: &ThemePalette,
        cx: &mut Context<Self>,
    ) -> Div {
        let tab = |id: &'static str, label: &'static str, target: IdentityTab| {
            let selected = self.tab == target;
            div()
                .id(id)
                .flex_1()
                .px_3()
                .py_2()
                .cursor_pointer()
                .text_sm()
                .bg(if selected {
                    palette.color(ThemeRole::ControlActive)
                } else {
                    palette.color(ThemeRole::ControlSurface)
                })
                .text_color(if selected {
                    palette.color(ThemeRole::ControlActiveText)
                } else {
                    palette.color(ThemeRole::TextSecondary)
                })
                .hover({
                    let hover = palette.color(ThemeRole::ControlSurfaceHover);
                    move |tab| tab.bg(hover)
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(target, cx)))
        };
        div()
            .w_full()
            .flex()
            .gap_1()
            .child(tab(
                "identity-tab-peer",
                "Their identity",
                IdentityTab::Peer,
            ))
            .child(tab(
                "identity-tab-local",
                "Your identity",
                IdentityTab::Local,
            ))
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(palette.color(ThemeRole::TextSubtle))
                    .child(format!(
                        "User ID {} | Room ID {:#x}",
                        document.peer.user_id.0, document.peer.room_id.0
                    )),
            )
    }

    fn render_words_header(
        &self,
        document: &wire::IdentityDocument,
        peer_tab: bool,
        palette: &ThemePalette,
        cx: &mut Context<Self>,
    ) -> Div {
        // The daemon decides how many words a key expands to, so every count the
        // dialog quotes is read off the document rather than assumed.
        let words = if peer_tab {
            document.peer.words.len()
        } else {
            document.local.words.len()
        };
        let heading = if peer_tab {
            format!("Identity words (compare all {words})")
        } else {
            format!("Your identity words (the other person compares all {words})")
        };
        let offer_confirmation = peer_tab && document.can_verify;
        let confirmed = self.words_confirmed;
        let confirmation_background = if confirmed {
            palette.color(ThemeRole::StateSuccess)
        } else {
            palette.color(ThemeRole::ControlSurface)
        };
        let confirmation_text = if confirmed {
            readable_button_text(confirmation_background, palette)
        } else {
            palette.color(ThemeRole::TextSecondary)
        };
        let confirmation_hover = if confirmed {
            highlighted_button_states(confirmation_background, palette).hover
        } else {
            ButtonTone {
                background: palette.color(ThemeRole::ControlSurfaceHover),
                foreground: palette.color(ThemeRole::TextSecondary),
            }
        };
        let confirmation_active = if confirmed {
            highlighted_button_states(confirmation_background, palette).active
        } else {
            ButtonTone {
                background: palette.color(ThemeRole::StatePressed),
                foreground: palette.color(ThemeRole::TextSecondary),
            }
        };
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .child(section_title(heading, palette).flex_1())
            .when(offer_confirmation, |header| {
                header.child(
                    div()
                        .id("identity-words-confirm")
                        .flex_none()
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .text_xs()
                        .bg(confirmation_background)
                        .text_color(confirmation_text)
                        .hover(move |button| {
                            button
                                .bg(confirmation_hover.background)
                                .text_color(confirmation_hover.foreground)
                        })
                        .active(move |button| {
                            button
                                .bg(confirmation_active.background)
                                .text_color(confirmation_active.foreground)
                        })
                        .child(if confirmed {
                            "Words matched ✓".to_string()
                        } else {
                            format!("I checked all {words} words")
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_words(cx))),
                )
            })
    }

    fn render_copy_row(
        &self,
        document: &wire::IdentityDocument,
        palette: &ThemePalette,
        cx: &mut Context<Self>,
    ) -> Div {
        let copy = |id: &'static str, label: &'static str, text: String| {
            div()
                .id(id)
                .px_3()
                .py_2()
                .cursor_pointer()
                .text_xs()
                .bg(palette.color(ThemeRole::ControlSurface))
                .text_color(palette.color(ThemeRole::TextSecondary))
                .hover({
                    let hover = palette.color(ThemeRole::ControlSurfaceHover);
                    move |button| button.bg(hover)
                })
                .flex()
                .items_center()
                .gap_2()
                .child(icon(
                    IconName::Copy,
                    14.,
                    palette.color(ThemeRole::TextMuted),
                ))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.copy(text.clone(), cx)))
        };
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(copy(
                "identity-copy-mine",
                "My verification text",
                document.local.verification_text.clone(),
            ))
            .child(copy(
                "identity-copy-key",
                "Their key",
                document.peer.public_key_hex.clone(),
            ))
            .child(copy(
                "identity-copy-words",
                "Their words",
                document.peer.words.join(" "),
            ))
    }

    fn render_verification_input(
        &self,
        words: usize,
        palette: &ThemePalette,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let failed = matches!(self.check, Some(wire::VerificationCheck::Invalid { .. }));
        let focused = self.paste.focus_handle(cx).is_focused(window);
        div()
            .flex_none()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(palette.color(ThemeRole::BorderSubtle))
            .flex()
            .flex_col()
            .gap_2()
            .child(paragraph(
                format!(
                    "Verification text is a copyable form of the same public identity, bound to \
                     this server and account. Paste their text below to check it automatically. \
                     If clipboard sharing is unavailable, compare the {words} words instead."
                ),
                palette,
            ))
            .child(
                div()
                    .w_full()
                    .min_h(rems_from_px(38.))
                    .px_3()
                    .py_2()
                    .border_1()
                    .border_color(if failed {
                        palette.color(ThemeRole::StateDanger)
                    } else if focused {
                        palette.color(ThemeRole::BorderFocus)
                    } else {
                        palette.color(ThemeRole::BorderStrong)
                    })
                    .bg(palette.color(ThemeRole::Input))
                    .child(self.paste.clone()),
            )
            .when_some(self.check.as_ref(), |column, check| {
                let (text, role) = match check {
                    wire::VerificationCheck::Match => (
                        "✓ Verification text matches this identity.".to_string(),
                        ThemeRole::StateSuccess,
                    ),
                    wire::VerificationCheck::Invalid { message, .. } => {
                        (message.clone(), ThemeRole::StateDanger)
                    }
                };
                column.child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(palette.color(role))
                        .child(text),
                )
            })
    }

    fn render_footer(
        &self,
        document: &wire::IdentityDocument,
        palette: &ThemePalette,
        cx: &mut Context<Self>,
    ) -> Div {
        let action = self.primary_action(document);
        let enabled = action.enabled();
        let background = action.tone(palette);
        let states = highlighted_button_states(background, palette);
        div()
            .flex_none()
            .px_5()
            .py_3()
            .flex()
            .items_center()
            .gap_3()
            .border_t_1()
            .border_color(palette.color(ThemeRole::BorderSubtle))
            .when_some(self.status.clone(), |footer, status| {
                footer.child(status_line(status, palette))
            })
            .child(div().flex_1())
            .child(
                div()
                    .id("identity-cancel")
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .bg(palette.color(ThemeRole::ControlSurface))
                    .text_color(palette.color(ThemeRole::TextSecondary))
                    .hover({
                        let hover = palette.color(ThemeRole::ControlSurfaceHover);
                        move |button| button.bg(hover)
                    })
                    .child("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .child(
                div()
                    .id("identity-primary")
                    .px_3()
                    .py_2()
                    .bg(states.rest.background)
                    .text_color(states.rest.foreground)
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |button| {
                                button
                                    .bg(states.hover.background)
                                    .text_color(states.hover.foreground)
                            })
                            .active(move |button| {
                                button
                                    .bg(states.active.background)
                                    .text_color(states.active.foreground)
                            })
                    })
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(action.label())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(document) = this.document.clone() else {
                            return;
                        };
                        if document.can_forget {
                            this.forget(cx);
                        } else {
                            this.verify(cx);
                        }
                    })),
            )
    }

    /// The one action the footer offers, which is never both verify and forget:
    /// an identity is either already confirmed or waiting to be.
    fn primary_action(&self, document: &wire::IdentityDocument) -> PrimaryAction {
        if document.can_forget {
            return PrimaryAction::Forget {
                confirming: self.forget_confirmation,
            };
        }
        if self.verification_passed() {
            return PrimaryAction::Verify;
        }
        if matches!(self.check, Some(wire::VerificationCheck::Invalid { .. })) {
            return PrimaryAction::Blocked(Blocked::CheckFailed);
        }
        PrimaryAction::Blocked(Blocked::Unconfirmed)
    }
}

/// The footer's single commit action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryAction {
    Verify,
    /// Dropping a confirmation is destructive, so it takes two presses.
    Forget {
        confirming: bool,
    },
    Blocked(Blocked),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blocked {
    CheckFailed,
    Unconfirmed,
}

impl PrimaryAction {
    fn label(self) -> &'static str {
        match self {
            Self::Verify => "Verify identity",
            Self::Forget { confirming: false } => "Forget verification",
            Self::Forget { confirming: true } => "Press again to forget verification",
            Self::Blocked(Blocked::CheckFailed) => "Clear verification text to continue",
            Self::Blocked(Blocked::Unconfirmed) => "Independent verification required",
        }
    }

    fn enabled(self) -> bool {
        !matches!(self, Self::Blocked(_))
    }

    fn tone(self, palette: &ThemePalette) -> gpui::Rgba {
        match self {
            Self::Verify => palette.color(ThemeRole::StateSuccess),
            Self::Forget { .. } => palette.color(ThemeRole::StateWarning),
            Self::Blocked(_) => palette.color(ThemeRole::StateDisabled),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ButtonTone {
    background: gpui::Rgba,
    foreground: gpui::Rgba,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HighlightedButtonStates {
    rest: ButtonTone,
    hover: ButtonTone,
    active: ButtonTone,
}

fn highlighted_button_states(
    background: gpui::Rgba,
    palette: &ThemePalette,
) -> HighlightedButtonStates {
    let rest_foreground = readable_button_text(background, palette);
    // Move each interaction state farther from its foreground. This preserves
    // the semantic status hue while increasing, rather than eroding, contrast.
    let tint = if relative_luminance(rest_foreground) < 0.5 {
        gpui::rgb(0xffffff)
    } else {
        gpui::rgb(0x000000)
    };
    let tone = |amount| {
        let background = background.blend(tint.alpha(amount));
        ButtonTone {
            background,
            foreground: readable_button_text(background, palette),
        }
    };
    HighlightedButtonStates {
        rest: ButtonTone {
            background,
            foreground: rest_foreground,
        },
        hover: tone(0.08),
        active: tone(0.16),
    }
}

/// Picks the higher-contrast neutral foreground for a solid status button.
///
/// State colors are user-configurable and may be either light or dark. They
/// can also be translucent, so compare against the color actually painted over
/// the dialog rather than assuming the configured state color is opaque.
fn readable_button_text(background: gpui::Rgba, palette: &ThemePalette) -> gpui::Rgba {
    let painted_background = palette.color(ThemeRole::Raised).blend(background);
    let dark = gpui::rgb(0x000000);
    let light = gpui::rgb(0xffffff);
    if contrast_ratio(painted_background, dark) >= contrast_ratio(painted_background, light) {
        dark
    } else {
        light
    }
}

fn contrast_ratio(first: gpui::Rgba, second: gpui::Rgba) -> f32 {
    let lighter = relative_luminance(first).max(relative_luminance(second));
    let darker = relative_luminance(first).min(relative_luminance(second));
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: gpui::Rgba) -> f32 {
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

fn status_banner(status: &wire::IdentityStatus, palette: &ThemePalette) -> Div {
    let role = match status.severity {
        wire::IdentitySeverity::Good => ThemeRole::StateSuccess,
        wire::IdentitySeverity::Warning => ThemeRole::StateWarning,
        wire::IdentitySeverity::Danger => ThemeRole::StateDanger,
    };
    div()
        .w_full()
        .font_weight(FontWeight::BOLD)
        .text_color(palette.color(role))
        .child(status.headline.clone())
}

fn status_line(status: SharedString, palette: &ThemePalette) -> Div {
    div()
        .min_w_0()
        .truncate()
        .text_xs()
        .text_color(palette.color(ThemeRole::TextMuted))
        .child(status)
}

fn section_title(text: impl Into<SharedString>, palette: &ThemePalette) -> Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(palette.color(ThemeRole::TextLink))
        .child(text.into())
}

fn paragraph(text: impl Into<SharedString>, palette: &ThemePalette) -> Div {
    div()
        .w_full()
        .text_sm()
        .text_color(palette.color(ThemeRole::TextBody))
        .child(text.into())
}

fn word_grid(
    words: &[String],
    code_family: SharedString,
    compact: bool,
    palette: &ThemePalette,
) -> Div {
    let columns = word_columns(compact);
    // The position and its word are one text run, not two elements: GPUI centers
    // every run in its own line box and quantizes glyph baselines to whole device
    // pixels (`SUBPIXEL_VARIANTS_Y == 1`), so two runs on one visual line drift
    // apart by a pixel on some rows and not others. One run has one baseline. The
    // monospace field width is what keeps the words in a column.
    let width = position_width(words.len());
    let position_style = HighlightStyle {
        color: Some(palette.color(ThemeRole::TextSubtle).into()),
        ..HighlightStyle::default()
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .font_family(code_family)
        .text_sm()
        .text_color(palette.color(ThemeRole::TextPrimary))
        .children(words.chunks(columns).enumerate().map(|(row, chunk)| {
            div()
                .w_full()
                .flex()
                .gap_2()
                .children(chunk.iter().enumerate().map(|(column, word)| {
                    let cell = numbered_word(row * columns + column, word, width);
                    div()
                        .flex_1()
                        .child(StyledText::new(cell).with_highlights([(0..width, position_style)]))
                }))
        }))
}

/// `word` prefixed by its 1-based position in a right-aligned `width`-wide
/// field. The position is what the two people read out to each other, so it is
/// numbered like the list the terminal dialog prints.
fn numbered_word(index: usize, word: &str, width: usize) -> SharedString {
    // `SharedString`'s `Display` writes through and drops the fill flags, so the
    // field has to be padded from the `&str` behind it.
    let position: &str = &word_position(index);
    format!("{position:>width$} {word}").into()
}

/// Columns the widest position label needs. All ASCII, so this is both the
/// character count the field pads to and the byte range the label occupies.
fn position_width(words: usize) -> usize {
    match words.checked_sub(1) {
        Some(last) => word_position(last).len(),
        None => 0,
    }
}

fn word_columns(compact: bool) -> usize {
    if compact {
        WORD_COLUMNS / 2
    } else {
        WORD_COLUMNS
    }
}

/// 1-based position labels for the word grid. The protocol caps a word list at
/// `MAX_IDENTITY_WORDS`, so every label the grid can ask for is in this table
/// and only the longer list a future protocol might send has to format one.
const WORD_POSITIONS: [&str; local_rpc::MAX_IDENTITY_WORDS] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16", "17",
    "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31", "32",
];

fn word_position(index: usize) -> SharedString {
    match WORD_POSITIONS.get(index) {
        Some(position) => SharedString::new_static(position),
        None => format!("{}", index + 1).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::ids::{RoomId, UserId};

    fn public_identity(user_id: u64, verification_text: &str) -> wire::PublicIdentity {
        wire::PublicIdentity {
            user_id: UserId(user_id),
            room_id: RoomId(0x8000_0001),
            public_key_hex: "bb".repeat(32),
            key_groups: vec!["bbbbbbbb".into(); 8],
            words: (0..24).map(|index| format!("word{index}")).collect(),
            verification_text: verification_text.into(),
        }
    }

    fn document(can_verify: bool, can_forget: bool) -> wire::IdentityDocument {
        wire::IdentityDocument {
            session_id: wire::IdentitySessionId(1),
            revision: 1,
            username: "zoe".into(),
            trust: wire::IdentityTrust::Unverified,
            status: wire::IdentityStatus {
                severity: wire::IdentitySeverity::Warning,
                headline: "UNVERIFIED: identity not independently confirmed".into(),
            },
            peer: public_identity(2, ""),
            local: public_identity(1, "chatt-e2e:v2:aaa:1:bbb:ccc"),
            can_verify,
            can_forget,
            error: None,
        }
    }

    fn create_identity(cx: &gpui::TestAppContext) -> Entity<IdentityView> {
        cx.update(|cx| {
            crate::fonts::init(cx);
            let config = crate::config::schema::GuiConfig::default();
            let available_families = cx.text_system().all_font_names();
            crate::theme::apply_appearance(
                &config,
                crate::config::io::SourceStatus::Missing,
                &[],
                &available_families,
                cx,
            );
            crate::settings::install_loaded(
                crate::config::io::LoadedConfig {
                    path: None,
                    config,
                    source: None,
                    status: crate::config::io::SourceStatus::Missing,
                    diagnostics: Vec::new(),
                },
                cx,
            );
            cx.new(IdentityView::new)
        })
    }

    #[test]
    fn highlighted_button_states_are_distinct_and_have_accessible_contrast() {
        let config = crate::config::schema::ThemeConfig::default();
        let palette = ThemePalette::from_config(&config);

        for role in [ThemeRole::StateSuccess, ThemeRole::StateWarning] {
            let background = palette.color(role);
            let states = highlighted_button_states(background, &palette);
            assert_ne!(states.rest.background, states.hover.background);
            assert_ne!(states.hover.background, states.active.background);

            for (state, tone) in [
                ("rest", states.rest),
                ("hover", states.hover),
                ("active", states.active),
            ] {
                let painted_background = palette.color(ThemeRole::Raised).blend(tone.background);
                let ratio = contrast_ratio(painted_background, tone.foreground);
                assert!(
                    ratio >= 4.5,
                    "{role:?} {state} contrast was only {ratio:.2}:1"
                );
            }
        }

        let disabled = palette.color(ThemeRole::StateDisabled);
        let disabled_background = palette.color(ThemeRole::Raised).blend(disabled);
        let disabled_foreground = readable_button_text(disabled, &palette);
        assert!(contrast_ratio(disabled_background, disabled_foreground) >= 4.5);
    }

    /// The word grid must never drop, duplicate, or misnumber a word: a reviewer
    /// who compared 23 of 24 would confirm a key they never fully checked. The
    /// daemon decides the length, so this holds for any list the wire allows.
    /// Each cell is one run of `<position><space><word>` in a fixed-width field,
    /// which is what puts every word of a column at the same offset.
    #[test]
    fn every_word_appears_exactly_once_and_in_position_at_both_widths() {
        for count in [1, 5, 24, local_rpc::MAX_IDENTITY_WORDS] {
            let words: Vec<String> = (0..count).map(|index| format!("word{index}")).collect();
            let width = position_width(count);
            for compact in [false, true] {
                let columns = word_columns(compact);
                let laid_out: Vec<&String> = words.chunks(columns).flatten().collect();
                assert_eq!(laid_out.len(), count, "{count} words, compact={compact}");
                for (index, word) in laid_out.into_iter().enumerate() {
                    assert_eq!(word, &format!("word{index}"));
                    let cell = numbered_word(index, word, width);
                    let (position, rest) = cell.split_at(width);
                    assert_eq!(position.trim_start(), format!("{}", index + 1));
                    assert_eq!(rest, format!(" word{index}"));
                }
            }
        }
    }

    /// Every count the dialog quotes comes off the document. A hardcoded 24
    /// would tell the reviewer to compare a number of words they were not shown.
    #[gpui::test]
    fn quoted_word_counts_follow_the_document(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                let mut short = document(true, false);
                short.peer.words.truncate(8);
                view.apply_document(short, cx);
                let document = view.document.clone().unwrap();
                assert_eq!(document.peer.words.len(), 8);
                assert_eq!(word_position(document.peer.words.len() - 1), "8");
            });
        });
    }

    /// Clearing the field must go through the daemon like every other edit.
    /// Dropping the verdict locally instead would let the reply for the text
    /// that was just deleted land afterwards and block the dialog on a verdict
    /// for an empty field.
    #[gpui::test]
    fn clearing_the_paste_field_still_asks_for_a_verdict(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        let commands = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let commands = commands.clone();
            cx.subscribe(&view, move |_, event: &IdentityViewEvent, _| {
                if let IdentityViewEvent::Command(command) = event {
                    commands.borrow_mut().push(command.clone());
                }
            })
        });

        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.apply_document(document(true, false), cx);
                view.check_text("chatt-e2e:v2:aaa:2:bbb:ccc".into(), cx);
                view.check_text(String::new(), cx);
            });
        });

        let sent = commands.borrow();
        assert_eq!(sent.len(), 2, "both edits are checked by the daemon");
        assert_eq!(
            sent[1],
            wire::IdentityCommand::CheckText {
                session_id: wire::IdentitySessionId(1),
                text: String::new(),
            }
        );
    }

    #[gpui::test]
    fn the_footer_offers_forget_only_for_a_confirmed_identity(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.apply_document(document(false, true), cx);
                let document = view.document.clone().unwrap();
                assert_eq!(
                    view.primary_action(&document),
                    PrimaryAction::Forget { confirming: false }
                );

                view.forget_confirmation = true;
                assert_eq!(
                    view.primary_action(&document),
                    PrimaryAction::Forget { confirming: true }
                );
            });
        });
    }

    #[gpui::test]
    fn verifying_stays_locked_until_words_or_text_are_confirmed(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.apply_document(document(true, false), cx);
                let document = view.document.clone().unwrap();
                assert_eq!(
                    view.primary_action(&document),
                    PrimaryAction::Blocked(Blocked::Unconfirmed),
                    "an unchecked identity cannot be verified"
                );

                view.check = Some(wire::VerificationCheck::Invalid {
                    danger: true,
                    message: "DANGER: verification text contains a different public key.".into(),
                });
                assert_eq!(
                    view.primary_action(&document),
                    PrimaryAction::Blocked(Blocked::CheckFailed),
                    "a failed check cannot be verified"
                );

                view.check = Some(wire::VerificationCheck::Match);
                assert_eq!(view.primary_action(&document), PrimaryAction::Verify);

                view.check = None;
                view.words_confirmed = true;
                assert_eq!(
                    view.primary_action(&document),
                    PrimaryAction::Verify,
                    "comparing the words is the offline path"
                );
            });
        });
    }

    /// A verdict belongs to the exact document it was computed against, so a key
    /// that moves mid-review cannot inherit the previous "match".
    #[gpui::test]
    fn a_check_for_a_superseded_document_is_discarded(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.apply_document(document(true, false), cx);

                view.apply_check(
                    wire::IdentitySessionId(1),
                    2,
                    wire::VerificationCheck::Match,
                    cx,
                );
                assert!(
                    view.check.is_none(),
                    "a later revision is not this document"
                );

                view.apply_check(
                    wire::IdentitySessionId(2),
                    1,
                    wire::VerificationCheck::Match,
                    cx,
                );
                assert!(view.check.is_none(), "another session is not this document");

                view.apply_check(
                    wire::IdentitySessionId(1),
                    1,
                    wire::VerificationCheck::Match,
                    cx,
                );
                assert_eq!(view.check, Some(wire::VerificationCheck::Match));
            });
        });
    }

    /// A key that changes under an open review must not keep the attestation
    /// the reviewer made about the previous key.
    #[gpui::test]
    fn a_new_revision_clears_local_attestations(cx: &mut gpui::TestAppContext) {
        let view = create_identity(cx);
        cx.update(|cx| {
            view.update(cx, |view, cx| {
                view.apply_document(document(true, false), cx);
                view.words_confirmed = true;
                view.check = Some(wire::VerificationCheck::Match);

                let mut moved = document(true, false);
                moved.revision = 2;
                view.apply_document(moved, cx);

                assert!(!view.words_confirmed);
                assert!(view.check.is_none());
                assert!(view.paste.read(cx).text().is_empty());
            });
        });
    }
}
