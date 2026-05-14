use super::{BalancerActionContext, DNVE3ConnectionBalancer};
use crate::action::OverlayAction;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::{ConnectionState, NodeId};
use std::collections::{HashMap, HashSet};

impl DNVE3ConnectionBalancer {
    /// 接続数が上限を超えているとき、スコアの低いノードを切断する。
    pub(super) fn prune_excess_connections(
        &self,
        connected: &mut Vec<NodeId>,
        ctx: &BalancerActionContext<'_>,
    ) -> Vec<OverlayAction> {
        if connected.len() <= ctx.target {
            return Vec::new();
        }

        let num_to_drop =
            (connected.len() - ctx.target) + ctx.config.limits.force_disconnect_count as usize;

        tracing::debug!(
            "[Balancer] over capacity: connected={} target={} will_drop={}",
            connected.len(),
            ctx.target,
            num_to_drop
        );

        let mut scores = Self::score_nodes(
            connected,
            ctx.selected_ids,
            ctx.self_pos,
            ctx.node_positions,
        );
        Self::sort_scores(&mut scores);

        let mut actions = Vec::new();
        let mut dropped = 0;
        for (id, score) in scores {
            if dropped >= num_to_drop {
                break;
            }
            if &id == ctx.self_id {
                continue;
            }
            match ctx.connected_map.get(&id) {
                Some(s)
                    if *s == ConnectionState::Connecting || *s == ConnectionState::Connected => {}
                _ => continue,
            }
            tracing::debug!(
                "[Balancer] DISCONNECT (over_capacity) {} score={:.3}",
                id.0,
                score
            );
            actions.push(OverlayAction::Disconnect { to: id.clone() });
            connected.retain(|x| x != &id);
            dropped += 1;
        }
        actions
    }

    /// 重要な方向のノードに接続し、必要なら最低スコアのノードと入れ替える。
    pub(super) fn connect_selected_nodes(
        &self,
        connected: &mut Vec<NodeId>,
        selected_nodes: &[NodeId],
        ctx: &BalancerActionContext<'_>,
    ) -> Vec<OverlayAction> {
        let max = ctx.config.limits.max_connection_count as usize;
        let mut actions = Vec::new();

        for id in selected_nodes {
            if id == ctx.self_id {
                continue;
            }
            match ctx.connected_map.get(id) {
                Some(s)
                    if *s == ConnectionState::Connecting || *s == ConnectionState::Connected =>
                {
                    continue;
                }
                _ => {}
            }

            if connected.len() >= max {
                let mut scores = Self::score_nodes(
                    connected,
                    ctx.selected_ids,
                    ctx.self_pos,
                    ctx.node_positions,
                );
                Self::sort_scores(&mut scores);

                if let Some((worst_id, worst_score)) = scores.first() {
                    let worst_id = worst_id.clone();
                    tracing::debug!(
                        "[Balancer] DISCONNECT (swap_for_better) {} score={:.3}",
                        worst_id.0,
                        worst_score
                    );
                    connected.retain(|x| x != &worst_id);
                    actions.push(OverlayAction::Disconnect { to: worst_id });
                } else {
                    tracing::debug!(
                        "[Balancer] mandatory directional node {} exceeds max_connection_count={}, keeping strong constraint",
                        id.0,
                        max
                    );
                }
            }

            tracing::debug!("[Balancer] CONNECT -> {}", id.0);
            actions.push(OverlayAction::Connect { to: id.clone() });
            connected.push(id.clone());
        }
        actions
    }

    fn score_nodes(
        nodes: &[NodeId],
        selected_ids: &HashSet<&NodeId>,
        self_pos: Vector3,
        node_positions: &HashMap<&NodeId, Vector3>,
    ) -> Vec<(NodeId, f32)> {
        nodes
            .iter()
            .filter(|id| !selected_ids.contains(id))
            .map(|id| {
                let score = node_positions
                    .get(id)
                    .map(|pos| Self::disconnect_score(self_pos, *pos))
                    .unwrap_or(f32::MIN);
                (id.clone(), score)
            })
            .collect()
    }

    fn disconnect_score(self_pos: Vector3, node_pos: Vector3) -> f32 {
        -(node_pos - self_pos).magnitude()
    }

    fn sort_scores(scores: &mut [(NodeId, f32)]) {
        scores.sort_by(|(a_id, a_score), (b_id, b_score)| {
            a_score.total_cmp(b_score).then_with(|| a_id.0.cmp(&b_id.0))
        });
    }
}
