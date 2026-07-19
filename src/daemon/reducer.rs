use rpc::daemon::{
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
    pub splices: Vec<(usize, usize, usize)>,
    pub request_resync: bool,
    pub request_result: Option<rpc::daemon::frame::RequestResult>,
}

pub fn apply(model: &mut ChatModel, frame: DaemonFrame) -> ReduceEffect {
    let mut effect = ReduceEffect::default();
    match frame {
        DaemonFrame::Welcome(welcome) => {
            model.daemon_instance = Some(welcome.instance_id);
            model.expected_seq = Some(welcome.first_event_seq);
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
                effect.request_resync = true;
                return effect;
            }
            install_snapshot(model, snapshot);
            model.expected_seq = Some(event_seq.wrapping_add(1));
            model.phase = ConnectionPhase::Ready;
            effect.replace_messages = true;
        }
        DaemonFrame::Event(event) => {
            if model.daemon_instance != Some(event.instance_id)
                || model.expected_seq != Some(event.event_seq)
            {
                model.phase = ConnectionPhase::Syncing;
                effect.request_resync = true;
                return effect;
            }
            model.expected_seq = Some(event.event_seq.wrapping_add(1));
            apply_delta(model, event.delta, &mut effect);
        }
        DaemonFrame::RequestResult(result) => {
            model.record_result(&result);
            effect.request_result = Some(result);
        }
        DaemonFrame::Pong { .. }
        | DaemonFrame::BulkStarted(_)
        | DaemonFrame::BulkChunk(_)
        | DaemonFrame::BulkFinished(_)
        | DaemonFrame::BulkCanceled { .. }
        | DaemonFrame::SharedMemoryOffer(_) => {}
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
    model.transfers = snapshot.transfers;
}

fn apply_delta(model: &mut ChatModel, delta: StateDelta, effect: &mut ReduceEffect) {
    match delta {
        StateDelta::RoomCatalogReset { rooms } => model.rooms = rooms,
        StateDelta::RoomUpserted { room } => {
            if let Some(existing) = model.rooms.iter_mut().find(|item| item.id == room.id) {
                *existing = room;
            } else {
                model.rooms.push(room);
                model.rooms.sort_by_key(|item| item.id);
            }
        }
        StateDelta::RoomRemoved { room_id } => model.rooms.retain(|room| room.id != room_id),
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
        StateDelta::ActiveRoomChanged { room_id } => model.selected_room = room_id,
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
        }
        StateDelta::MessagesPrepended {
            messages,
            older_cursor,
            at_start,
            ..
        } => {
            model.older_cursor = older_cursor;
            model.at_start = at_start;
            let mut incoming: Vec<_> = messages.into_iter().map(timeline::from_daemon).collect();
            incoming.retain(|message| !model.messages.iter().any(|item| item.id == message.id));
            let added = incoming.len();
            incoming.append(&mut model.messages);
            model.messages = incoming;
            if added > 0 {
                effect.splices.push((0, 0, added));
            }
        }
        StateDelta::MessageUpserted { message } => {
            let message = timeline::from_daemon(message);
            if let Some(existing) = model.messages.iter_mut().find(|item| item.id == message.id) {
                *existing = message;
            } else {
                let index = model.messages.len();
                model.messages.push(message);
                effect.splices.push((index, index, 1));
            }
        }
        StateDelta::MessageDeleted { message_id, .. } => {
            if let Some(index) = model
                .messages
                .iter()
                .position(|message| message.id == message_id.0)
            {
                model.messages.remove(index);
                effect.splices.push((index, index + 1, 0));
            }
        }
        StateDelta::VoiceStateChanged { voice } => model.voice = voice,
        StateDelta::ResyncRequired { .. } => {
            model.phase = ConnectionPhase::Syncing;
            effect.request_resync = true;
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
            if matches!(
                transfer.status,
                rpc::daemon::model::TransferStatus::Complete
                    | rpc::daemon::model::TransferStatus::Canceled
                    | rpc::daemon::model::TransferStatus::Failed
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
        StateDelta::ParticipantsChanged { participants, .. } => model.participants = participants,
        StateDelta::ShareAvailable { .. }
        | StateDelta::ShareConfig { .. }
        | StateDelta::ShareEnded { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::daemon::{
        bulk::BulkTransport,
        frame::{NegotiatedLimits, StateEvent, Welcome},
        model::{ConnectionState, DaemonInstanceId, FrontendClientId, VoiceState},
    };

    fn welcome(instance: DaemonInstanceId) -> DaemonFrame {
        DaemonFrame::Welcome(Welcome {
            version: 1,
            capabilities: Vec::new(),
            client_id: FrontendClientId(1),
            instance_id: instance,
            daemon_build: "test".into(),
            bulk_transport: BulkTransport::RpcChunksV1,
            connection: ConnectionState::Online,
            active_server: Some("local".into()),
            first_event_seq: 4,
            limits: NegotiatedLimits::default(),
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
        }
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
        assert_eq!(model.phase, ConnectionPhase::Ready);
        assert_eq!(model.expected_seq, Some(5));
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
    }
}
