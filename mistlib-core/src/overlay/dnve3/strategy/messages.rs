use super::DNVE3Strategy;
use crate::action::OverlayAction;
use crate::config::ConnectionMode;
use crate::types::NodeId;

impl DNVE3Strategy {
    pub(super) fn mark_peer_seen(&self, from: &NodeId) {
        let mut store = self.data_store.lock().expect("data_store lock poisoned");
        store.update_last_message_time(from);
    }

    pub(super) fn handle_heartbeat_message(
        &self,
        from: &NodeId,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        if self.density_heartbeat_enabled() {
            self.exchanger.handle_heartbeat(from.clone(), payload);
        }
        vec![]
    }

    pub(super) fn handle_request_node_list_message(
        &self,
        from: &NodeId,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        self.exchanger.handle_request_node_list_with_payload(
            from.clone(),
            self.current_connection_mode(),
            payload,
        )
    }

    pub(super) fn handle_node_list_message(
        &self,
        from: &NodeId,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        self.exchanger.handle_node_list(from, payload);
        vec![]
    }

    pub(super) fn handle_position_message(
        &self,
        from: &NodeId,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        if self.position_heartbeat_enabled() {
            self.exchanger.handle_position(from.clone(), payload);
        }
        vec![]
    }

    fn density_heartbeat_enabled(&self) -> bool {
        matches!(
            self.current_connection_mode(),
            ConnectionMode::DirectionDensity
                | ConnectionMode::DirectionDensityLight
                | ConnectionMode::NodeListAoiDensity
        )
    }

    fn position_heartbeat_enabled(&self) -> bool {
        matches!(
            self.current_connection_mode(),
            ConnectionMode::NodeListDirectional
                | ConnectionMode::NodeListAoiGuard
                | ConnectionMode::NodeListAoiProximity
                | ConnectionMode::NodeListAoiDensity
                | ConnectionMode::NodeListProximity
                | ConnectionMode::PSense
        )
    }
}
