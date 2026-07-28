use super::*;

impl ChattView {
    pub(super) fn completion_view(&self, cx: &Context<Self>) -> Option<CompletionView> {
        let snapshot = self.composer.read(cx).snapshot();
        let context = completion::completion_context(
            &snapshot.text,
            snapshot.selection,
            snapshot.accepts_completion,
            snapshot.composing,
            &self.model.commands,
        )?;
        if !matches!(context, CompletionContext::Emoji { .. })
            && (!self.model.is_ready()
                || self.editing.is_some()
                || self.pending_command.is_some()
                || self.file_inspection_pending
                || self.pending_submission.is_some())
        {
            return None;
        }
        let context_key = completion::context_key(&context);
        let (options, hint) = match &context {
            CompletionContext::Command { query, .. } => {
                let options = completion::command_options(&self.model.commands, query);
                let hint = options
                    .is_empty()
                    .then(|| "No matching commands · Enter runs as typed".into());
                (options, hint)
            }
            CompletionContext::Argument {
                command,
                kind: ArgumentKind::Free,
                query,
                ..
            } => {
                let hint = query
                    .is_empty()
                    .then(|| {
                        command
                            .placeholder
                            .clone()
                            .unwrap_or_else(|| command.usage.clone())
                    })
                    .map(SharedString::from);
                (Vec::new(), hint)
            }
            CompletionContext::Argument {
                kind: ArgumentKind::Candidates(kind),
                query,
                ..
            } => {
                let cached = self.command_candidates.get(kind);
                let options = cached
                    .map(|items| completion::candidate_options(*kind, items, query))
                    .unwrap_or_default();
                let hint = if cached.is_none() && self.candidate_requests.contains_key(kind) {
                    Some(format!("Loading {}…", candidate_kind_label(*kind)).into())
                } else if options.is_empty() {
                    Some(format!("No matching {}", candidate_kind_label(*kind)).into())
                } else {
                    None
                };
                (options, hint)
            }
            CompletionContext::Emoji { query, .. } => (completion::emoji_options(query), None),
        };
        Some(CompletionView {
            context,
            context_key,
            options,
            hint,
        })
    }

    pub(super) fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let view = self.completion_view(cx);
        let had_completion = self.completion_session.is_some();
        let previous_key = self
            .completion_session
            .as_ref()
            .map(|session| session.context_key.clone());
        let request_kind = view.as_ref().and_then(|view| match &view.context {
            CompletionContext::Argument {
                kind: ArgumentKind::Candidates(kind),
                ..
            } if previous_key.as_deref() != Some(view.context_key.as_str()) => Some(*kind),
            _ => None,
        });

        match &view {
            None => self.completion_session = None,
            Some(view) if previous_key.as_deref() != Some(view.context_key.as_str()) => {
                self.completion_session = Some(completion::open_session(&view.context));
            }
            Some(view) => completion::reconcile_session(
                &mut self.completion_session,
                Some(&view.context),
                &view.options,
            ),
        }
        if let Some(view) = &view
            && matches!(view.context, CompletionContext::Emoji { .. })
            && let Some(session) = &mut self.completion_session
        {
            completion::engage_first(session, &view.options);
        }
        if let Some(kind) = request_kind {
            self.request_command_candidates(kind);
        }
        let completion_open = view.as_ref().is_some_and(|view| {
            self.completion_session
                .as_ref()
                .is_some_and(|session| session.context_key == view.context_key)
                && (!view.options.is_empty() || view.hint.is_some())
        });
        let completion_engaged = completion_open
            && self
                .completion_session
                .as_ref()
                .is_some_and(|session| session.engaged);
        self.composer.update(cx, |composer, _| {
            composer.set_completion_state(completion_open, completion_engaged)
        });
        if had_completion || view.is_some() {
            cx.notify();
        }
    }

    fn request_command_candidates(&mut self, kind: CommandCandidateKind) {
        if self.command_candidates.contains_key(&kind)
            || self.candidate_requests.contains_key(&kind)
        {
            return;
        }
        let request_id = self.request_id();
        self.candidate_requests.insert(kind, request_id);
        if let Err(error) = self
            .daemon
            .send(ClientFrame::RequestCommandCandidates { request_id, kind })
        {
            self.candidate_requests.remove(&kind);
            self.status = error.into();
        }
    }

    pub(super) fn clear_completion(&mut self, cx: &mut Context<Self>) {
        self.completion_session = None;
        self.command_candidates.clear();
        self.candidate_requests.clear();
        self.composer
            .update(cx, |composer, _| composer.set_completion_open(false));
    }

    pub(super) fn clear_command_surface(&mut self, cx: &mut Context<Self>) {
        self.pending_command = None;
        self.clear_completion(cx);
        self.formatted_command_messages.clear();
        if !self.command_rows.is_empty() {
            self.command_rows.clear();
            self.rebuild_message_list();
        }
    }

    pub(super) fn append_command_output(&mut self, lines: Vec<CommandOutputLine>) {
        if lines.is_empty() {
            return;
        }
        let anchor_message_id = self.model.messages.last().map(|message| message.id);
        let timestamp_ms = timeline::now_ms();
        for line in lines {
            let local_id = self.next_command_row_id.max(1);
            self.next_command_row_id = if local_id == u64::MAX {
                1
            } else {
                local_id + 1
            };
            let body = line.text;
            self.formatted_command_messages
                .insert(local_id, Rc::new(FormattedMessage::plain(body.clone())));
            self.command_rows.push(timeline::LocalCommandRow {
                local_id,
                anchor_message_id,
                body,
                error: line.error,
                timestamp_ms,
            });
        }
        self.rebuild_message_list();
        self.list_state.scroll_to_end();
    }

    fn move_completion(&mut self, delta: isize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.completion_view(cx) else {
            self.completion_session = None;
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            return;
        };
        let Some(session) = &mut self.completion_session else {
            return;
        };
        if completion::move_selection(session, &view.options, delta) {
            self.composer
                .update(cx, |composer, _| composer.set_completion_state(true, true));
            cx.notify();
        }
    }

    pub(super) fn completion_next(
        &mut self,
        _: &CompletionNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_completion(1, window, cx);
    }

    pub(super) fn completion_previous(
        &mut self,
        _: &CompletionPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_completion(-1, window, cx);
    }

    pub(super) fn completion_accept(
        &mut self,
        _: &CompletionAccept,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(session) = &self.completion_session else {
            return;
        };
        let Some(option) = completion::tab_option(session, &view.options).cloned() else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    pub(super) fn completion_accept_engaged(
        &mut self,
        _: &CompletionAcceptEngaged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(session) = &self.completion_session else {
            return;
        };
        let Some(option) = completion::enter_option(session, &view.options).cloned() else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    pub(super) fn completion_dismiss(
        &mut self,
        _: &CompletionDismiss,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_session.take().is_some() {
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            cx.notify();
        }
    }

    fn hover_completion(&mut self, key: OptionKey, cx: &mut Context<Self>) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        if !view.options.iter().any(|option| option.key == key) {
            return;
        }
        let Some(session) = &mut self.completion_session else {
            return;
        };
        session.active = Some(key);
        session.engaged = true;
        self.composer
            .update(cx, |composer, _| composer.set_completion_state(true, true));
        cx.notify();
    }

    fn accept_completion_key(
        &mut self,
        key: OptionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.completion_view(cx) else {
            return;
        };
        let Some(option) = view
            .options
            .iter()
            .find(|option| option.key == key)
            .cloned()
        else {
            return;
        };
        self.accept_completion_option(&view.context, option, window, cx);
    }

    fn accept_completion_option(
        &mut self,
        context: &CompletionContext,
        option: CompletionOption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reopen = matches!(
            &option.value,
            CompletionValue::Command(command)
                if command.arg != local_rpc::model::CommandArgKind::None
        );
        let replacement = completion::replacement(context, &option);
        self.suppress_completion_refresh = true;
        self.composer.update(cx, |composer, cx| {
            composer.replace_completion(replacement.span, &replacement.text, window, cx)
        });
        self.suppress_completion_refresh = false;
        self.completion_session = None;
        if reopen {
            self.refresh_completion(cx);
        } else {
            self.composer
                .update(cx, |composer, _| composer.set_completion_open(false));
            cx.notify();
        }
    }

    pub(super) fn render_completion_popup(
        &mut self,
        view: CompletionView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = AppliedSettings::get(cx);
        let emoji_context = matches!(&view.context, CompletionContext::Emoji { .. });
        let active = self
            .completion_session
            .as_ref()
            .and_then(|session| session.active.clone());
        let mut rows = div().w_full().flex().flex_col();

        for (index, option) in view.options.into_iter().enumerate() {
            let selected = active.as_ref() == Some(&option.key);
            let hover_key = option.key.clone();
            let accept_key = option.key.clone();
            let content = match &option.value {
                CompletionValue::Command(command) => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        highlighted_completion_label(
                            command.name.clone(),
                            &option.match_ranges,
                            selected,
                            &settings.theme,
                        )
                        .max_w(rems_from_px(138.))
                        .min_w_0()
                        .flex_1(),
                    )
                    .child(
                        div()
                            .max_w(rems_from_px(190.))
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextSecondary
                            }))
                            .child(command.usage.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextDim
                            }))
                            .child(command.description.clone()),
                    ),
                CompletionValue::Candidate { kind, item } => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .max_w(rems_from_px(72.))
                            .flex_none()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextSubtle
                            }))
                            .child(candidate_kind_label(*kind)),
                    )
                    .child(
                        highlighted_completion_label(
                            item.value.clone(),
                            &option.match_ranges,
                            selected,
                            &settings.theme,
                        )
                        .max_w(rems_from_px(220.))
                        .min_w_0()
                        .flex_1(),
                    )
                    .when_some(item.detail.clone(), |row, detail| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(settings.theme.color(if selected {
                                    ThemeRole::TextInverse
                                } else {
                                    ThemeRole::TextDim
                                }))
                                .child(detail),
                        )
                    }),
                CompletionValue::Emoji(record) => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(rems_from_px(48.))
                            .flex_none()
                            .text_xl()
                            .child(record.unicode.clone()),
                    )
                    .child(
                        div()
                            .max_w(rems_from_px(220.))
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextPrimary
                            }))
                            .child(record.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(settings.theme.color(if selected {
                                ThemeRole::TextInverse
                            } else {
                                ThemeRole::TextDim
                            }))
                            .child(format!(":{}:", record.shortcode)),
                    ),
            };
            rows = rows.child(
                div()
                    .id(("completion-option", index))
                    .min_h(rems_from_px(40.))
                    .w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(settings.theme.color(if selected {
                        ThemeRole::BorderFocus
                    } else {
                        ThemeRole::BorderStrong
                    }))
                    .bg(settings.theme.color(if selected {
                        ThemeRole::ControlActive
                    } else {
                        ThemeRole::Raised
                    }))
                    .hover({
                        let hover = settings.theme.color(if selected {
                            ThemeRole::ControlActive
                        } else {
                            ThemeRole::StateHover
                        });
                        move |row| row.bg(hover)
                    })
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        this.hover_completion(hover_key.clone(), cx)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accept_completion_key(accept_key.clone(), window, cx)
                    }))
                    .child(content),
            );
        }

        if let Some(hint) = view.hint {
            rows = rows.child(
                div()
                    .min_h(rems_from_px(40.))
                    .w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_b_1()
                    .border_color(settings.theme.color(ThemeRole::BorderStrong))
                    .text_sm()
                    .text_color(settings.theme.color(ThemeRole::TextMuted))
                    .child(hint),
            );
        }

        div()
            .id("command-completion")
            .absolute()
            .left(rems_from_px(79.))
            .right(rems_from_px(79.))
            .bottom(relative(1.))
            .mb(rems_from_px(6.))
            .border_1()
            .border_color(settings.theme.color(ThemeRole::BorderStrong))
            .bg(settings.theme.color(ThemeRole::Raised))
            .child(rows)
            .child(
                div()
                    .min_h(rems_from_px(28.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .bg(settings.theme.color(ThemeRole::Sidebar))
                    .text_xs()
                    .text_color(settings.theme.color(ThemeRole::TextDim))
                    .child("↑↓ navigate  ·  Tab complete")
                    .child(if emoji_context {
                        "Enter insert  ·  Esc close"
                    } else {
                        "Enter run  ·  Esc close"
                    }),
            )
            .with_animation(
                ("command-completion-in", 0usize),
                Animation::new(Duration::from_millis(90)).with_easing(gpui::ease_out_quint()),
                |popup, delta| popup.opacity(delta).mb(rems_from_px(2. + 4. * delta)),
            )
            .into_any_element()
    }
}
