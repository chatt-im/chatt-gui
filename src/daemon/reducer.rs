use local_rpc::{
    frame::{DaemonFrame, StateDelta},
    model::StateSnapshot,
};

use crate::{
    model::{ChatModel, ConnectionPhase},
    timeline,
};

#[derive(Debug, Default)]
pub struct ReduceEffect {
    pub replace_messages: bool,
    pub messages_changed: bool,
    pub splices: Vec<(usize, usize, usize)>,
    pub request_resync: bool,
    pub request_result: Option<local_rpc::frame::RequestResult>,
    pub command_result: Option<(
        local_rpc::frame::RequestResult,
        Vec<local_rpc::model::CommandOutputLine>,
    )>,
    pub command_candidates: Option<(
        local_rpc::model::RequestId,
        local_rpc::model::CommandCandidateKind,
        Vec<local_rpc::model::CommandCandidate>,
    )>,
}

pub fn apply(model: &mut ChatModel, frame: DaemonFrame) -> ReduceEffect {
    let mut effect = ReduceEffect::default();
    match frame {
        DaemonFrame::Welcome(welcome) => {
            model.daemon_instance = Some(welcome.instance_id);
            model.expected_seq = Some(welcome.first_event_seq);
            model.resync_requested = false;
            model.limits = welcome.limits;
            model.commands = welcome.commands;
            model.active_server = welcome.active_server;
            model.server_connection = welcome.connection;
            model.phase = ConnectionPhase::Syncing;
        }
        DaemonFrame::Snapshot {
            instance_id,
            event_seq,
            snapshot,
        } => {
            if model.daemon_instance != Some(instance_id) {
                model.phase = ConnectionPhase::Syncing;
                effect.request_resync = !model.resync_requested;
                model.resync_requested = true;
                return effect;
            }
            install_snapshot(model, snapshot);
            model.expected_seq = Some(event_seq.wrapping_add(1));
            model.resync_requested = false;
            model.phase = ConnectionPhase::Ready;
            effect.replace_messages = true;
            effect.messages_changed = true;
        }
        DaemonFrame::Event(event) => {
            if model.resync_requested {
                return effect;
            }
            if model.daemon_instance != Some(event.instance_id)
                || model.expected_seq != Some(event.event_seq)
            {
                model.phase = ConnectionPhase::Syncing;
                effect.request_resync = !model.resync_requested;
                model.resync_requested = true;
                return effect;
            }
            model.expected_seq = Some(event.event_seq.wrapping_add(1));
            apply_delta(model, event.delta, &mut effect);
        }
        DaemonFrame::RequestResult(result) => {
            model.record_result(&result);
            effect.request_result = Some(result);
        }
        DaemonFrame::CommandResult { result, lines } => {
            model.record_result(&result);
            effect.command_result = Some((result, lines));
        }
        DaemonFrame::CommandCandidates {
            request_id,
            kind,
            items,
        } => {
            effect.command_candidates = Some((request_id, kind, items));
        }
        DaemonFrame::Pong { .. }
        | DaemonFrame::LiveShareOpened { .. }
        | DaemonFrame::BulkChunk(_)
        | DaemonFrame::BulkFinished(_)
        | DaemonFrame::BulkCanceled { .. } => {}
    }
    effect
}

fn install_snapshot(model: &mut ChatModel, snapshot: StateSnapshot) {
    model.active_server = snapshot.active_server;
    model.server_connection = snapshot.connection;
    model.local_identity = snapshot.local_identity;
    model.rooms = snapshot.rooms;
    model.selected_room = snapshot.selected_room;
    if let Some(room) = snapshot.room {
        model.older_cursor = room.older_cursor;
        model.at_start = room.at_start;
        model.participants = room.participants;
        model.messages = room
            .messages
            .into_iter()
            .map(timeline::from_daemon)
            .collect();
    } else {
        model.older_cursor = None;
        model.at_start = true;
        model.messages.clear();
        model.participants.clear();
    }
    model.voice = snapshot.voice;
    model.transfers = snapshot
        .transfers
        .into_iter()
        .filter(|transfer| {
            !matches!(
                transfer.status,
                local_rpc::model::TransferStatus::Complete
                    | local_rpc::model::TransferStatus::Canceled
                    | local_rpc::model::TransferStatus::Failed
            )
        })
        .collect();
    model.live_shares = snapshot.live_shares;
}

fn apply_delta(model: &mut ChatModel, delta: StateDelta, effect: &mut ReduceEffect) {
    match delta {
        StateDelta::RoomCatalogReset { rooms } => {
            if model
                .selected_room
                .is_some_and(|selected| !rooms.iter().any(|room| room.id == selected))
            {
                request_resync(model, effect);
                return;
            }
            model.rooms = rooms;
        }
        StateDelta::RoomUpserted { room } => {
            if let Some(existing) = model.rooms.iter_mut().find(|item| item.id == room.id) {
                *existing = room;
            } else {
                model.rooms.push(room);
                model.rooms.sort_by_key(|item| item.id);
            }
        }
        StateDelta::RoomRemoved { room_id } => {
            model.rooms.retain(|room| room.id != room_id);
            if model.selected_room == Some(room_id) {
                model.selected_room = None;
                clear_room_state(model, effect);
            }
        }
        StateDelta::RoomUnreadChanged {
            room_id,
            unread,
            behind_head,
        } => {
            if let Some(room) = model.rooms.iter_mut().find(|room| room.id == room_id) {
                room.unread = unread;
                room.behind_head = behind_head;
            }
        }
        StateDelta::ActiveRoomChanged { room_id } => {
            let changed = model.selected_room != room_id;
            model.selected_room = room_id;
            if changed {
                clear_room_state(model, effect);
            }
        }
        StateDelta::RoomSnapshot(room) => {
            model.selected_room = Some(room.room_id);
            model.older_cursor = room.older_cursor;
            model.at_start = room.at_start;
            model.participants = room.participants;
            model.messages = room
                .messages
                .into_iter()
                .map(timeline::from_daemon)
                .collect();
            effect.replace_messages = true;
            effect.messages_changed = true;
        }
        StateDelta::MessagesPrepended {
            room_id,
            messages,
            older_cursor,
            at_start,
        } => {
            if !is_active_room(model, room_id, effect) {
                return;
            }
            model.older_cursor = older_cursor;
            model.at_start = at_start;
            let mut incoming: Vec<_> = messages.into_iter().map(timeline::from_daemon).collect();
            let first_existing = model.messages.first().map(|message| message.id);
            if incoming.iter().any(|message| {
                first_existing.is_some_and(|first| {
                    message.id >= first
                        && model
                            .messages
                            .binary_search_by_key(&message.id, |item| item.id)
                            .is_err()
                })
            }) {
                request_resync(model, effect);
                return;
            }
            incoming.retain(|message| {
                model
                    .messages
                    .binary_search_by_key(&message.id, |item| item.id)
                    .is_err()
            });
            let added = incoming.len();
            incoming.append(&mut model.messages);
            model.messages = incoming;
            if added > 0 {
                effect.messages_changed = true;
                effect.splices.push((0, 0, added));
            }
        }
        StateDelta::HistoryStateChanged {
            room_id,
            older_cursor,
            at_start,
        } => {
            if !is_active_room(model, room_id, effect) {
                return;
            }
            model.older_cursor = older_cursor;
            model.at_start = at_start;
        }
        StateDelta::MessageUpserted { message } => {
            if !is_active_room(model, message.room_id, effect) {
                return;
            }
            let message = timeline::from_daemon(message);
            effect.messages_changed = true;
            match model
                .messages
                .binary_search_by_key(&message.id, |item| item.id)
            {
                Ok(index) => model.messages[index] = message,
                Err(index) => {
                    model.messages.insert(index, message);
                    effect.splices.push((index, index, 1));
                }
            }
        }
        StateDelta::MessageDeleted {
            room_id,
            message_id,
        } => {
            if !is_active_room(model, room_id, effect) {
                return;
            }
            if let Some(index) = model
                .messages
                .binary_search_by_key(&message_id.0, |message| message.id)
                .ok()
            {
                model.messages.remove(index);
                effect.messages_changed = true;
                effect.splices.push((index, index + 1, 0));
            }
        }
        StateDelta::VoiceStateChanged { voice } => model.voice = voice,
        StateDelta::LiveShareUpserted { share } => {
            match model
                .live_shares
                .binary_search_by_key(&share.stream_id, |item| item.stream_id)
            {
                Ok(index) => model.live_shares[index] = share,
                Err(index) => model.live_shares.insert(index, share),
            }
        }
        StateDelta::LiveShareRemoved { stream_id } => {
            if let Ok(index) = model
                .live_shares
                .binary_search_by_key(&stream_id, |item| item.stream_id)
            {
                model.live_shares.remove(index);
            }
        }
        StateDelta::ResyncRequired { .. } => {
            model.phase = ConnectionPhase::Syncing;
            effect.request_resync = !model.resync_requested;
            model.resync_requested = true;
        }
        StateDelta::DaemonStopping => {
            model.phase = ConnectionPhase::Disconnected {
                reason: "daemon stopped".into(),
            }
        }
        StateDelta::ConnectionChanged {
            connection,
            active_server,
        } => {
            model.server_connection = connection;
            model.active_server = active_server;
        }
        StateDelta::SecurityChanged { room_id, trust } => {
            if let Some(room) = model.rooms.iter_mut().find(|room| room.id == room_id) {
                room.trust = trust;
            }
        }
        StateDelta::TransferChanged { transfer } => {
            if !is_active_room(model, transfer.room_id, effect) {
                return;
            }
            if matches!(
                transfer.status,
                local_rpc::model::TransferStatus::Complete
                    | local_rpc::model::TransferStatus::Canceled
                    | local_rpc::model::TransferStatus::Failed
            ) {
                model
                    .transfers
                    .retain(|item| item.transfer_id != transfer.transfer_id);
            } else if let Some(existing) = model
                .transfers
                .iter_mut()
                .find(|item| item.transfer_id == transfer.transfer_id)
            {
                *existing = transfer;
            } else {
                model.transfers.push(transfer);
            }
        }
        StateDelta::TransferRemoved { transfer_id } => {
            model
                .transfers
                .retain(|transfer| transfer.transfer_id != transfer_id);
        }
        StateDelta::ParticipantsChanged {
            room_id,
            participants,
        } => {
            if is_active_room(model, room_id, effect) {
                model.participants = participants;
            }
        }
    }
}

fn clear_room_state(model: &mut ChatModel, effect: &mut ReduceEffect) {
    model.older_cursor = None;
    model.at_start = true;
    model.messages.clear();
    model.participants.clear();
    model.transfers.clear();
    effect.replace_messages = true;
    effect.messages_changed = true;
}

fn is_active_room(
    model: &mut ChatModel,
    room_id: local_rpc::ids::RoomId,
    effect: &mut ReduceEffect,
) -> bool {
    if model.selected_room == Some(room_id) {
        true
    } else {
        request_resync(model, effect);
        false
    }
}

fn request_resync(model: &mut ChatModel, effect: &mut ReduceEffect) {
    model.phase = ConnectionPhase::Syncing;
    effect.request_resync = !model.resync_requested;
    model.resync_requested = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::ids::{RoomId, UserId};
    use local_rpc::{
        frame::{NegotiatedLimits, Operation, RequestOutcome, RequestResult, StateEvent, Welcome},
        model::{
            CommandArgKind, CommandCandidate, CommandCandidateKind, CommandInfo, CommandOutputLine,
            ConnectionState, DaemonInstanceId, Participant, RequestId, VoiceState,
        },
    };

    fn welcome(instance: DaemonInstanceId) -> DaemonFrame {
        DaemonFrame::Welcome(Welcome {
            version: local_rpc::PROTOCOL_MAX_VERSION,
            instance_id: instance,
            daemon_build: "test".into(),
            connection: ConnectionState::Online,
            active_server: Some("local".into()),
            first_event_seq: 4,
            limits: NegotiatedLimits::default(),
            commands: Vec::new(),
        })
    }

    fn snapshot() -> StateSnapshot {
        StateSnapshot {
            connection: ConnectionState::Online,
            active_server: Some("local".into()),
            local_identity: Some("alice".into()),
            rooms: Vec::new(),
            selected_room: None,
            room: None,
            voice: VoiceState {
                muted: false,
                deafened: false,
                output_volume: 100.0,
                joined_room: None,
            },
            transfers: Vec::new(),
            live_shares: Vec::new(),
        }
    }

    fn message(room_id: RoomId, message_id: u64) -> local_rpc::model::Message {
        local_rpc::model::Message {
            room_id,
            message_id: local_rpc::ids::MessageId(message_id),
            sender_id: UserId(1),
            sender_name: "alice".into(),
            body: message_id.to_string(),
            timestamp_ms: message_id,
            local: false,
            edited: false,
            unverified: false,
            notice: false,
            reference: None,
            attachment: None,
        }
    }

    #[test]
    fn live_share_deltas_upsert_and_remove_in_stream_order() {
        let mut model = ChatModel::default();
        let mut effect = ReduceEffect::default();
        for stream_id in [9, 3] {
            apply_delta(
                &mut model,
                StateDelta::LiveShareUpserted {
                    share: local_rpc::model::LiveShare {
                        room_id: RoomId(1),
                        stream_id: local_rpc::ids::StreamId(stream_id),
                        sender_name: "alice".into(),
                        codec: "avc1.42C00D".into(),
                        coded_width: 320,
                        coded_height: 240,
                        extradata: vec![1],
                    },
                },
                &mut effect,
            );
        }
        assert_eq!(
            model
                .live_shares
                .iter()
                .map(|share| share.stream_id.0)
                .collect::<Vec<_>>(),
            vec![3, 9]
        );
        apply_delta(
            &mut model,
            StateDelta::LiveShareRemoved {
                stream_id: local_rpc::ids::StreamId(3),
            },
            &mut effect,
        );
        assert_eq!(model.live_shares[0].stream_id.0, 9);
    }

    #[test]
    fn welcome_then_snapshot_installs_sequence_baseline() {
        let instance = DaemonInstanceId([3; 16]);
        let mut model = ChatModel::default();
        apply(&mut model, welcome(instance));
        let effect = apply(
            &mut model,
            DaemonFrame::Snapshot {
                instance_id: instance,
                event_seq: 4,
                snapshot: snapshot(),
            },
        );
        assert!(effect.replace_messages);
        assert!(effect.messages_changed);
        assert_eq!(model.phase, ConnectionPhase::Ready);
        assert_eq!(model.expected_seq, Some(5));
    }

    #[test]
    fn welcome_installs_the_daemon_command_catalog() {
        let instance = DaemonInstanceId([4; 16]);
        let mut model = ChatModel::default();
        let DaemonFrame::Welcome(mut frame) = welcome(instance) else {
            unreachable!();
        };
        frame.commands.push(CommandInfo {
            name: "/whoami".into(),
            usage: "/whoami".into(),
            description: "show identity".into(),
            arg: CommandArgKind::None,
            placeholder: None,
        });

        apply(&mut model, DaemonFrame::Welcome(frame));

        assert_eq!(model.commands.len(), 1);
        assert_eq!(model.commands[0].name, "/whoami");
    }

    #[test]
    fn command_terminal_frames_are_returned_as_typed_effects() {
        let request_id = RequestId(8);
        let mut model = ChatModel::default();
        model.pending.insert(
            request_id,
            crate::model::PendingRequest {
                operation: Operation::RunCommand,
                room_id: None,
                draft: Some("/whoami".into()),
                transfer_id: None,
            },
        );
        let result = RequestResult {
            request_id,
            operation: Operation::RunCommand,
            outcome: RequestOutcome::Accepted,
        };
        let lines = vec![CommandOutputLine {
            error: false,
            text: "alice".into(),
        }];

        let effect = apply(
            &mut model,
            DaemonFrame::CommandResult {
                result: result.clone(),
                lines: lines.clone(),
            },
        );

        assert_eq!(effect.command_result, Some((result, lines)));
        assert!(!model.pending.contains_key(&request_id));

        let items = vec![CommandCandidate {
            value: "alice".into(),
            detail: None,
        }];
        let effect = apply(
            &mut model,
            DaemonFrame::CommandCandidates {
                request_id,
                kind: CommandCandidateKind::User,
                items: items.clone(),
            },
        );
        assert_eq!(
            effect.command_candidates,
            Some((request_id, CommandCandidateKind::User, items))
        );
    }

    #[test]
    fn sequence_gap_stops_delta_application_and_requests_resync() {
        let instance = DaemonInstanceId([3; 16]);
        let mut model = ChatModel::default();
        apply(&mut model, welcome(instance));
        apply(
            &mut model,
            DaemonFrame::Snapshot {
                instance_id: instance,
                event_seq: 4,
                snapshot: snapshot(),
            },
        );
        let effect = apply(
            &mut model,
            DaemonFrame::Event(StateEvent {
                instance_id: instance,
                event_seq: 6,
                delta: StateDelta::DaemonStopping,
            }),
        );
        assert!(effect.request_resync);
        assert_eq!(model.phase, ConnectionPhase::Syncing);

        let repeated = apply(
            &mut model,
            DaemonFrame::Event(StateEvent {
                instance_id: instance,
                event_seq: 7,
                delta: StateDelta::DaemonStopping,
            }),
        );
        assert!(!repeated.request_resync);
    }

    #[test]
    fn clearing_active_room_clears_room_scoped_state() {
        let instance = DaemonInstanceId([3; 16]);
        let mut model = ChatModel::default();
        apply(&mut model, welcome(instance));
        apply(
            &mut model,
            DaemonFrame::Snapshot {
                instance_id: instance,
                event_seq: 4,
                snapshot: snapshot(),
            },
        );
        model.selected_room = Some(RoomId(1));
        model.at_start = false;
        model.participants.push(Participant {
            user_id: UserId(1),
            name: "alice".into(),
            online: true,
            speaking: false,
            muted: false,
            deafened: false,
        });
        let effect = apply(
            &mut model,
            DaemonFrame::Event(StateEvent {
                instance_id: instance,
                event_seq: 5,
                delta: StateDelta::ActiveRoomChanged { room_id: None },
            }),
        );
        assert!(effect.replace_messages);
        assert!(effect.messages_changed);
        assert_eq!(model.selected_room, None);
        assert!(model.at_start);
        assert!(model.participants.is_empty());
    }

    #[test]
    fn rejects_delta_for_a_different_active_room() {
        let instance = DaemonInstanceId([3; 16]);
        let mut model = ChatModel::default();
        apply(&mut model, welcome(instance));
        apply(
            &mut model,
            DaemonFrame::Snapshot {
                instance_id: instance,
                event_seq: 4,
                snapshot: snapshot(),
            },
        );
        model.selected_room = Some(RoomId(1));
        let effect = apply(
            &mut model,
            DaemonFrame::Event(StateEvent {
                instance_id: instance,
                event_seq: 5,
                delta: StateDelta::MessageUpserted {
                    message: message(RoomId(2), 1),
                },
            }),
        );
        assert!(effect.request_resync);
        assert!(model.messages.is_empty());
    }

    #[test]
    fn inserts_message_upserts_in_id_order() {
        let instance = DaemonInstanceId([3; 16]);
        let mut model = ChatModel::default();
        apply(&mut model, welcome(instance));
        apply(
            &mut model,
            DaemonFrame::Snapshot {
                instance_id: instance,
                event_seq: 4,
                snapshot: snapshot(),
            },
        );
        model.selected_room = Some(RoomId(1));
        model.messages = vec![
            timeline::from_daemon(message(RoomId(1), 1)),
            timeline::from_daemon(message(RoomId(1), 3)),
        ];
        let effect = apply(
            &mut model,
            DaemonFrame::Event(StateEvent {
                instance_id: instance,
                event_seq: 5,
                delta: StateDelta::MessageUpserted {
                    message: message(RoomId(1), 2),
                },
            }),
        );
        assert!(effect.messages_changed);
        assert_eq!(
            model
                .messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
