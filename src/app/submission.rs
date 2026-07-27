use super::*;

impl ChattView {
    pub(super) fn begin_submission(
        &mut self,
        room_id: RoomId,
        draft: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let files = VecDeque::from(self.queued_files.take_all());
        let total_files = files.len();
        let phase = if draft.is_some() {
            let request_id = self.request_id();
            SubmissionPhase::AwaitingMessage { request_id }
        } else {
            SubmissionPhase::ReadyForUpload
        };
        self.pending_submission = Some(PendingSubmission {
            room_id,
            draft: draft.clone(),
            files,
            total_files,
            completed_files: 0,
            phase,
        });

        let Some(draft) = draft else {
            self.start_next_submission_upload(cx);
            return;
        };
        let SubmissionPhase::AwaitingMessage { request_id } = self
            .pending_submission
            .as_ref()
            .expect("submission was just installed")
            .phase
            .clone()
        else {
            unreachable!("message submission starts by waiting for its request")
        };
        self.model.pending.insert(
            request_id,
            PendingRequest {
                operation: Operation::SendMessage,
                room_id: Some(room_id),
                draft: Some(draft.clone()),
                transfer_id: None,
            },
        );
        if let Err(error) = self.daemon.send(ClientFrame::SendMessage {
            request_id,
            room_id,
            body: draft,
        }) {
            self.model.pending.remove(&request_id);
            self.fail_pending_submission(error, cx);
        } else {
            self.status = if total_files == 0 {
                "Sending message…".into()
            } else {
                format!("Sending message before {total_files} queued files…").into()
            };
            cx.notify();
        }
    }

    fn start_next_submission_upload(&mut self, cx: &mut Context<Self>) {
        let Some(mut submission) = self.pending_submission.take() else {
            return;
        };
        let Some(file) = submission.files.pop_front() else {
            self.status = if submission.total_files == 0 {
                "Message accepted".into()
            } else {
                format!(
                    "{} {} submitted",
                    submission.total_files,
                    if submission.total_files == 1 {
                        "file"
                    } else {
                        "files"
                    }
                )
                .into()
            };
            cx.notify();
            return;
        };

        let begin_request = self.request_id();
        let finish_request = self.request_id();
        let transfer_id = self.transfer_id();
        let room_id = submission.room_id;
        let file_name = file.file_name.clone();
        let source = file.source.clone();
        submission.phase = SubmissionPhase::Uploading(SubmittedUpload {
            file,
            begin_request,
            finish_request,
        });
        let position = submission.completed_files + 1;
        let total = submission.total_files;
        self.pending_submission = Some(submission);
        self.model.pending.insert(
            begin_request,
            PendingRequest {
                operation: Operation::BeginUpload,
                room_id: Some(room_id),
                draft: None,
                transfer_id: Some(transfer_id),
            },
        );
        self.model.pending.insert(
            finish_request,
            PendingRequest {
                operation: Operation::FinishUpload,
                room_id: Some(room_id),
                draft: None,
                transfer_id: Some(transfer_id),
            },
        );
        let upload_queued = match source {
            QueuedFileSource::Path(path) => {
                self.daemon.upload_file(
                    path,
                    room_id,
                    transfer_id,
                    begin_request,
                    finish_request,
                    self.model.limits.upload_bytes,
                );
                Ok(())
            }
            QueuedFileSource::Memory(bytes) => self.daemon.upload_bytes(
                bytes,
                file_name.clone(),
                room_id,
                transfer_id,
                begin_request,
                finish_request,
                self.model.limits.upload_bytes,
            ),
        };
        if let Err(error) = upload_queued {
            self.fail_pending_submission(error, cx);
            return;
        }
        self.status = format!("Sending file {position} of {total} · {file_name}").into();
        cx.notify();
    }

    pub(super) fn handle_submission_result(
        &mut self,
        result: &local_rpc::frame::RequestResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let phase = self
            .pending_submission
            .as_ref()
            .map(|submission| submission.phase.clone());
        match phase {
            Some(SubmissionPhase::AwaitingMessage { request_id })
                if request_id == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        let draft = self
                            .pending_submission
                            .as_mut()
                            .and_then(|submission| submission.draft.take());
                        if let Some(draft) = draft
                            && self.composer.read(cx).text() == draft
                        {
                            self.composer.update(cx, |composer, cx| composer.clear(cx));
                        }
                        if let Some(submission) = self.pending_submission.as_mut() {
                            submission.phase = SubmissionPhase::ReadyForUpload;
                        }
                        self.start_next_submission_upload(cx);
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            Some(SubmissionPhase::Uploading(upload))
                if upload.begin_request == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        if let Some(submission) = self.pending_submission.as_ref() {
                            self.status = format!(
                                "Sending file {} of {} · {}",
                                submission.completed_files + 1,
                                submission.total_files,
                                upload.file.file_name
                            )
                            .into();
                        }
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            Some(SubmissionPhase::Uploading(upload))
                if upload.finish_request == result.request_id =>
            {
                match &result.outcome {
                    RequestOutcome::Accepted => {
                        if let Some(submission) = self.pending_submission.as_mut() {
                            submission.completed_files += 1;
                            submission.phase = SubmissionPhase::ReadyForUpload;
                        }
                        self.start_next_submission_upload(cx);
                    }
                    RequestOutcome::Rejected { message, .. } => {
                        self.fail_pending_submission(message.clone(), cx);
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn pending_submission_matches_upload(
        &self,
        begin_request: RequestId,
        finish_request: RequestId,
    ) -> bool {
        matches!(
            self.pending_submission
                .as_ref()
                .map(|submission| &submission.phase),
            Some(SubmissionPhase::Uploading(upload))
                if upload.begin_request == begin_request
                    && upload.finish_request == finish_request
        )
    }

    pub(super) fn fail_pending_submission(&mut self, reason: String, cx: &mut Context<Self>) {
        let Some(submission) = self.pending_submission.take() else {
            self.set_composer_error(format!("Upload failed · {reason}"), cx);
            return;
        };
        if let Some(draft) = submission.draft.as_ref()
            && self.composer.read(cx).text().is_empty()
        {
            self.composer
                .update(cx, |composer, cx| composer.restore(draft.clone(), cx));
        }
        let files = self.recover_failed_submission_files(submission);
        self.queued_files.restore(files);
        self.set_composer_error(format!("Could not submit · {reason}"), cx);
    }

    pub(super) fn abandon_disconnected_submission(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(submission) = self.pending_submission.take() else {
            return self.submission_outcome_unknown;
        };
        let ambiguous = submission.outcome_is_ambiguous();
        if let Some(draft) = submission.draft.as_ref()
            && self.composer.read(cx).text().is_empty()
        {
            self.composer
                .update(cx, |composer, cx| composer.restore(draft.clone(), cx));
        }
        let files = self.recover_failed_submission_files(submission);
        self.queued_files.restore(files);
        self.submission_outcome_unknown |= ambiguous;
        self.submission_outcome_unknown
    }

    fn recover_failed_submission_files(
        &mut self,
        submission: PendingSubmission,
    ) -> Vec<QueuedFile> {
        let (first_request, second_request) = submission.request_ids();
        if let Some(request_id) = first_request {
            self.model.pending.remove(&request_id);
        }
        if let Some(request_id) = second_request {
            self.model.pending.remove(&request_id);
        }
        submission.into_failed_files()
    }
}
