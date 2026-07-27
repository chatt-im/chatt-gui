use super::*;

impl ChattView {
    pub(super) fn update_code_search(&mut self, cx: &mut Context<Self>) {
        if !self.code_search_open {
            return;
        }
        let Some((document, _, _)) = self.active_code_document() else {
            self.close_code_search(cx);
            return;
        };
        let query = {
            let input = self.code_search_input.read(cx);
            input.text()
        };
        self.code_search_generation = self.code_search_generation.wrapping_add(1);
        let generation = self.code_search_generation;
        self.code_search_task.take();
        self.code_search_results = CodeSearchResults::default();
        self.code_search_result_index = 0;
        self.code_search_pending = !query.is_empty();
        if query.is_empty() {
            cx.notify();
            return;
        }

        let executor = cx.background_executor().clone();
        self.code_search_task = Some(cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(75)).await;
            let results = executor.spawn(async move { document.search(&query) }).await;
            let _ = this.update(cx, |this, cx| {
                if !this.code_search_open || this.code_search_generation != generation {
                    return;
                }
                this.code_search_task.take();
                this.code_search_pending = false;
                this.code_search_results = results;
                this.code_search_result_index = 0;
                this.scroll_to_code_match(ScrollStrategy::Center);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn scroll_to_code_match(&self, strategy: ScrollStrategy) {
        let Some(search_match) = self.code_search_results.get(self.code_search_result_index) else {
            return;
        };
        if let Some((document, scroll_handle, view_state)) = self.active_code_document() {
            let target = document.match_target(search_match);
            view_state.request_match_reveal(search_match);
            scroll_handle.scroll_to_item_strict(target.line, strategy);
        }
    }

    pub(super) fn open_code_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_code_document().is_none() {
            return;
        }
        self.code_search_open = true;
        self.update_code_search(cx);
        window.focus(&self.code_search_input.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn next_code_match(&mut self, cx: &mut Context<Self>) {
        if self.code_search_results.is_empty() {
            return;
        }
        self.code_search_result_index =
            (self.code_search_result_index + 1) % self.code_search_results.len();
        self.scroll_to_code_match(ScrollStrategy::Center);
        cx.notify();
    }

    pub(super) fn previous_code_match(&mut self, cx: &mut Context<Self>) {
        if self.code_search_results.is_empty() {
            return;
        }
        self.code_search_result_index = self
            .code_search_result_index
            .checked_sub(1)
            .unwrap_or(self.code_search_results.len() - 1);
        self.scroll_to_code_match(ScrollStrategy::Center);
        cx.notify();
    }

    pub(super) fn close_code_search(&mut self, cx: &mut Context<Self>) {
        if !self.code_search_open
            && !self.code_search_pending
            && self.code_search_results.is_empty()
        {
            return;
        }
        self.code_search_generation = self.code_search_generation.wrapping_add(1);
        self.code_search_task.take();
        self.code_search_open = false;
        self.code_search_pending = false;
        self.code_search_results = CodeSearchResults::default();
        self.code_search_result_index = 0;
        cx.notify();
    }

    pub(super) fn find_in_code_action(
        &mut self,
        _: &FindInCode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.open_code_search(window, cx);
    }

    pub(super) fn next_code_match_action(
        &mut self,
        _: &NextCodeMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.next_code_match(cx);
    }

    pub(super) fn previous_code_match_action(
        &mut self,
        _: &PreviousCodeMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.previous_code_match(cx);
    }

    pub(super) fn close_code_search_action(
        &mut self,
        _: &CloseCodeSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.close_code_search(cx);
        window.focus(&self.code_viewer_focus, cx);
    }
}
