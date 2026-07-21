use std::collections::HashMap;

use local_rpc::{
    frame::{NegotiatedLimits, Operation, RequestOutcome},
    ids::RoomId,
    model::{
        ConnectionState, DaemonInstanceId, LiveShare, Participant, RequestId, RoomSummary,
        TransferSummary, VoiceState,
    },
};

use crate::timeline::Message;

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionPhase {
    Discovering,
    Connecting,
    Syncing,
    Ready,
    Disconnected { reason: String },
    Incompatible { details: String },
}

#[derive(Clone, Debug)]
pub struct PendingRequest {
    pub operation: Operation,
    pub room_id: Option<RoomId>,
    pub draft: Option<String>,
    pub transfer_id: Option<local_rpc::model::BulkTransferId>,
}

pub struct ChatModel {
    pub phase: ConnectionPhase,
    pub daemon_instance: Option<DaemonInstanceId>,
    pub expected_seq: Option<u64>,
    pub resync_requested: bool,
    pub limits: NegotiatedLimits,
    pub active_server: Option<String>,
    pub server_connection: ConnectionState,
    pub local_identity: Option<String>,
    pub rooms: Vec<RoomSummary>,
    pub selected_room: Option<RoomId>,
    pub messages: Vec<Message>,
    pub participants: Vec<Participant>,
    pub older_cursor: Option<local_rpc::ids::MessageId>,
    pub at_start: bool,
    pub voice: VoiceState,
    pub transfers: Vec<TransferSummary>,
    pub live_shares: Vec<LiveShare>,
    pub pending: HashMap<RequestId, PendingRequest>,
    pub last_error: Option<String>,
}

impl Default for ChatModel {
    fn default() -> Self {
        Self {
            phase: ConnectionPhase::Discovering,
            daemon_instance: None,
            expected_seq: None,
            resync_requested: false,
            limits: NegotiatedLimits::default(),
            active_server: None,
            server_connection: ConnectionState::Offline,
            local_identity: None,
            rooms: Vec::new(),
            selected_room: None,
            messages: Vec::new(),
            participants: Vec::new(),
            older_cursor: None,
            at_start: true,
            voice: VoiceState {
                muted: false,
                deafened: false,
                output_volume: 100.0,
                joined_room: None,
            },
            transfers: Vec::new(),
            live_shares: Vec::new(),
            pending: HashMap::new(),
            last_error: None,
        }
    }
}

impl ChatModel {
    pub fn is_ready(&self) -> bool {
        self.phase == ConnectionPhase::Ready
    }

    pub fn selected_room(&self) -> Option<&RoomSummary> {
        let selected = self.selected_room?;
        self.rooms.iter().find(|room| room.id == selected)
    }

    pub fn record_result(
        &mut self,
        result: &local_rpc::frame::RequestResult,
    ) -> Option<PendingRequest> {
        let pending = self.pending.remove(&result.request_id);
        if let RequestOutcome::Rejected { message, .. } = &result.outcome {
            self.last_error = Some(message.clone());
        } else {
            self.last_error = None;
        }
        pending
    }
}
