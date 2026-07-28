use super::signature::{generate_secret_key_bytes, xonly_public_key_hex};
use super::util::{hex_encode, now_unix_seconds};
use crate::error::{MistError, Result};
use crate::types::NodeId;
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct SignalingSecretKey([u8; 32]);

impl SignalingSecretKey {
    pub fn generate() -> Self {
        Self(generate_secret_key_bytes())
    }

    pub fn from_bytes_for_tests(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SignalingSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignalingSecretKey(<redacted>)")
    }
}

#[derive(Clone)]
pub struct TemporarySignalingIdentity {
    pub public_key: String,
    secret_key: SignalingSecretKey,
}

impl TemporarySignalingIdentity {
    pub fn generate() -> Self {
        Self::from_secret_key(SignalingSecretKey::generate())
    }

    pub fn from_secret_key(secret_key: SignalingSecretKey) -> Self {
        let public_key = xonly_public_key_hex(secret_key.as_bytes())
            .expect("SignalingSecretKey must be a valid secp256k1 secret key");
        Self {
            public_key,
            secret_key,
        }
    }

    pub fn secret_key(&self) -> &SignalingSecretKey {
        &self.secret_key
    }

    pub fn short_public_key(&self) -> String {
        hex_encode(&self.public_key.as_bytes()[..8.min(self.public_key.len())])
    }
}

impl fmt::Debug for TemporarySignalingIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TemporarySignalingIdentity")
            .field("public_key", &self.public_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryEntry {
    pub signaling_pubkey: String,
    pub expires_at: u64,
    pub topology_rank: String,
}

/// A node id's binding to a signaling pubkey, plus the peer-declared session
/// epoch (typically the `joined_at` timestamp carried on discovery/message
/// envelopes) that binding was last seen with. `epoch` is `None` when the
/// peer never supplied one (e.g. a legacy sender, or the binding was created
/// via one of the epoch-agnostic `bind_node*` wrappers).
#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeBinding {
    pubkey: String,
    epoch: Option<u64>,
}

/// Outcome of binding a node id to a signaling pubkey.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindOutcome {
    /// Already bound to this same pubkey.
    Known,
    /// First time this node id is bound.
    New,
    /// The node id was bound to a *different* pubkey and the change was
    /// accepted: the peer restarted (browser reload) with a fresh signaling
    /// identity under the same node id.
    Rebound { previous_pubkey: String },
}

impl BindOutcome {
    /// True only for [`BindOutcome::Known`].
    pub fn is_known(&self) -> bool {
        matches!(self, BindOutcome::Known)
    }

    /// The pubkey this node id was bound to before the rebind, if this
    /// outcome is a [`BindOutcome::Rebound`].
    pub fn rebound_from(&self) -> Option<&str> {
        match self {
            BindOutcome::Rebound { previous_pubkey } => Some(previous_pubkey.as_str()),
            _ => None,
        }
    }
}

fn newer_epoch(stored: Option<u64>, incoming: Option<u64>) -> Option<u64> {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => Some(stored.max(incoming)),
        (Some(stored), None) => Some(stored),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

#[derive(Default, Debug)]
pub struct DiscoveryTable {
    by_pubkey: HashMap<String, DiscoveryEntry>,
    node_to_pubkey: HashMap<NodeId, NodeBinding>,
}

impl DiscoveryTable {
    pub fn insert_pubkey(&mut self, signaling_pubkey: String, expires_at: u64) {
        let topology_rank = signaling_pubkey.clone();
        self.insert_pubkey_with_rank(signaling_pubkey, expires_at, topology_rank);
    }

    pub fn insert_pubkey_with_rank(
        &mut self,
        signaling_pubkey: String,
        expires_at: u64,
        topology_rank: String,
    ) {
        self.sweep_expired(now_unix_seconds());
        let expires_at = self
            .by_pubkey
            .get(&signaling_pubkey)
            .map_or(expires_at, |entry| entry.expires_at.max(expires_at));
        self.by_pubkey.insert(
            signaling_pubkey.clone(),
            DiscoveryEntry {
                signaling_pubkey,
                expires_at,
                topology_rank,
            },
        );
    }

    pub fn bind_node(&mut self, node_id: NodeId, signaling_pubkey: String, expires_at: u64) {
        let _ = self.bind_node_checked(node_id, signaling_pubkey, expires_at);
    }

    pub fn bind_node_checked(
        &mut self,
        node_id: NodeId,
        signaling_pubkey: String,
        expires_at: u64,
    ) -> Result<bool> {
        let topology_rank = signaling_pubkey.clone();
        self.bind_node_checked_with_rank(node_id, signaling_pubkey, expires_at, topology_rank)
    }

    pub fn bind_node_checked_with_rank(
        &mut self,
        node_id: NodeId,
        signaling_pubkey: String,
        expires_at: u64,
        topology_rank: String,
    ) -> Result<bool> {
        self.bind_node_checked_with_rank_and_rebind(
            node_id,
            signaling_pubkey,
            expires_at,
            topology_rank,
            false,
        )
    }

    /// Thin wrapper over [`bind_node_with_epoch`](Self::bind_node_with_epoch)
    /// that never supplies a session epoch. Kept byte-for-byte identical in
    /// behavior to its pre-epoch implementation for existing callers
    /// (mistlib-native, mistlib-wasm): with `sender_epoch: None`, a pubkey
    /// change is only ever accepted when `allow_rebind` is true, exactly as
    /// before.
    pub fn bind_node_checked_with_rank_and_rebind(
        &mut self,
        node_id: NodeId,
        signaling_pubkey: String,
        expires_at: u64,
        topology_rank: String,
        allow_rebind: bool,
    ) -> Result<bool> {
        self.bind_node_with_epoch(
            node_id,
            signaling_pubkey,
            expires_at,
            topology_rank,
            None,
            allow_rebind,
        )
        .map(|outcome| outcome.is_known())
    }

    /// Binds `node_id` to `signaling_pubkey`, taking a peer-declared session
    /// epoch (`sender_epoch`, the `joined_at` timestamp carried on a
    /// discovery/message envelope) into account when the node id is already
    /// bound to a *different* pubkey.
    ///
    /// If the node id is unbound, the binding is created (`New`). If it is
    /// already bound to the same pubkey, the stored epoch is advanced to the
    /// newer of the stored and incoming epoch (`Known`). If it is bound to a
    /// *different* pubkey, the rebind is accepted (`Rebound`) when either:
    ///   a. `sender_epoch` is strictly greater than the stored epoch (or the
    ///      stored epoch is absent) — a genuinely newer session announcing
    ///      itself, e.g. a browser peer that reloaded and regenerated its
    ///      temporary signaling keypair while keeping the same host-assigned
    ///      `NodeId`; or
    ///   b. `allow_rebind` is set, preserving the pre-epoch escape hatch.
    /// Otherwise the pubkey change is rejected with the pre-existing
    /// `"Nostr sender node id changed pubkey"` error.
    ///
    /// Security note: path (a) intentionally trusts a peer-declared,
    /// unauthenticated `joined_at` value — a hostile room member could send
    /// a large `joined_at` to steal an existing node id's binding away from
    /// its legitimate owner. This is an accepted trade-off, not an
    /// oversight: the pre-existing guard was already weak (any member can
    /// freely claim an *unbound* node id, and only the "already bound to a
    /// different pubkey" case was ever guarded), and dropping every message
    /// from a validly-reloaded peer for the full discovery TTL (minutes) is
    /// a hard availability failure in exchange for a marginal hardening of
    /// an already-soft guard. Epochs must be STRICTLY greater than the
    /// stored value to win a rebind; a tie is rejected (unless
    /// `allow_rebind` is set), so a replayed/duplicate `joined_at` cannot
    /// steal a binding.
    pub fn bind_node_with_epoch(
        &mut self,
        node_id: NodeId,
        signaling_pubkey: String,
        expires_at: u64,
        topology_rank: String,
        sender_epoch: Option<u64>,
        allow_rebind: bool,
    ) -> Result<BindOutcome> {
        self.sweep_expired(now_unix_seconds());

        let (outcome, epoch) = match self.node_to_pubkey.get(&node_id) {
            None => (BindOutcome::New, sender_epoch),
            Some(existing) if existing.pubkey == signaling_pubkey => (
                BindOutcome::Known,
                newer_epoch(existing.epoch, sender_epoch),
            ),
            Some(existing) => {
                let previous_pubkey = existing.pubkey.clone();
                let stored_epoch = existing.epoch;
                let epoch_is_newer = matches!(
                    sender_epoch,
                    Some(candidate) if stored_epoch.is_none_or(|stored| candidate > stored)
                );
                if epoch_is_newer || allow_rebind {
                    self.by_pubkey.remove(&previous_pubkey);
                    (BindOutcome::Rebound { previous_pubkey }, sender_epoch)
                } else {
                    return Err(MistError::Signaling(
                        "Nostr sender node id changed pubkey".to_string(),
                    ));
                }
            }
        };

        self.insert_pubkey_with_rank(signaling_pubkey.clone(), expires_at, topology_rank);
        self.node_to_pubkey.insert(
            node_id,
            NodeBinding {
                pubkey: signaling_pubkey,
                epoch,
            },
        );
        Ok(outcome)
    }

    pub fn pubkey_for_node(&mut self, node_id: &NodeId) -> Option<String> {
        self.sweep_expired(now_unix_seconds());
        self.node_to_pubkey
            .get(node_id)
            .map(|binding| binding.pubkey.clone())
    }

    /// The session epoch (peer-declared `joined_at`) last recorded for
    /// `node_id`'s current binding, if any. `None` both when the node id is
    /// unbound and when it is bound but no epoch has ever been supplied for
    /// it.
    pub fn epoch_for_node(&self, node_id: &NodeId) -> Option<u64> {
        self.node_to_pubkey
            .get(node_id)
            .and_then(|binding| binding.epoch)
    }

    /// Removes `node_id`'s binding and its backing `by_pubkey` entry,
    /// returning the pubkey it was bound to.
    pub fn unbind_node(&mut self, node_id: &NodeId) -> Option<String> {
        let binding = self.node_to_pubkey.remove(node_id)?;
        self.by_pubkey.remove(&binding.pubkey);
        Some(binding.pubkey)
    }

    /// Renews the discovery entry backing an already-bound node using the
    /// receiver's own clock (`now + ttl_seconds`), without altering the
    /// pubkey binding itself.
    ///
    /// Discovery-table entries otherwise only get their `expires_at` pushed
    /// forward by periodic discovery re-announcements (every ~ttl/2). If a
    /// single re-announcement is missed (e.g. during relay reconnect churn),
    /// a peer we are actively exchanging signaling messages with can still
    /// lapse out of `node_to_pubkey` and start failing with `RouteNotFound`.
    /// Call this whenever a signaling message from `node_id` has just been
    /// validated and accepted, so an active peer's entry cannot expire out
    /// from under an in-progress exchange (e.g. an ICE-restart negotiation).
    ///
    /// No-ops if `node_id` is not currently bound to a pubkey; this never
    /// creates a binding and never changes which pubkey a node is bound to,
    /// so it does not weaken the pubkey-change rejection guard in
    /// `bind_node_checked_with_rank_and_rebind`.
    pub fn touch_node(&mut self, node_id: &NodeId, ttl_seconds: u64) {
        let now = now_unix_seconds();
        self.sweep_expired(now);
        let Some(pubkey) = self
            .node_to_pubkey
            .get(node_id)
            .map(|binding| binding.pubkey.clone())
        else {
            return;
        };
        if let Some(entry) = self.by_pubkey.get_mut(&pubkey) {
            entry.expires_at = entry.expires_at.max(now.saturating_add(ttl_seconds));
        }
    }

    pub fn active_pubkeys(&mut self) -> Vec<String> {
        self.sweep_expired(now_unix_seconds());
        self.by_pubkey.keys().cloned().collect()
    }

    pub fn responder_pubkeys_for(
        &mut self,
        subject_pubkey: &str,
        subject_rank: &str,
        local_pubkey: &str,
        local_rank: &str,
        limit: usize,
    ) -> Vec<String> {
        self.sweep_expired(now_unix_seconds());
        if limit == 0 {
            return Vec::new();
        }

        let mut ranked: Vec<(String, String)> = self
            .by_pubkey
            .values()
            .map(|entry| (entry.topology_rank.clone(), entry.signaling_pubkey.clone()))
            .collect();

        if !ranked.iter().any(|(_, pubkey)| pubkey == local_pubkey) {
            ranked.push((local_rank.to_string(), local_pubkey.to_string()));
        }
        if !ranked.iter().any(|(_, pubkey)| pubkey == subject_pubkey) {
            ranked.push((subject_rank.to_string(), subject_pubkey.to_string()));
        }

        ranked.sort();
        ranked.dedup_by(|left, right| left.1 == right.1);
        if ranked.len() <= 1 {
            return Vec::new();
        }

        let Some(subject_index) = ranked
            .iter()
            .position(|(_, pubkey)| pubkey == subject_pubkey)
        else {
            return Vec::new();
        };

        let mut responders = Vec::with_capacity(limit);
        for offset in 1..ranked.len() {
            let index = (subject_index + ranked.len() - offset) % ranked.len();
            let pubkey = &ranked[index].1;
            if pubkey == subject_pubkey {
                continue;
            }
            responders.push(pubkey.clone());
            if responders.len() == limit {
                break;
            }
        }
        responders
    }

    pub fn expires_at_for_pubkey(&self, signaling_pubkey: &str) -> Option<u64> {
        self.by_pubkey
            .get(signaling_pubkey)
            .map(|entry| entry.expires_at)
    }

    pub fn sweep_expired(&mut self, now: u64) {
        self.by_pubkey.retain(|_, entry| entry.expires_at > now);
        self.node_to_pubkey
            .retain(|_, binding| self.by_pubkey.contains_key(&binding.pubkey));
    }

    pub fn clear(&mut self) {
        self.by_pubkey.clear();
        self.node_to_pubkey.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{now_unix_seconds, BindOutcome, DiscoveryTable};
    use crate::types::NodeId;
    use std::time::Duration;

    #[test]
    fn touch_node_renews_entry_past_its_original_expiry() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-active".to_string());
        let near_expiry = now_unix_seconds() + 1;
        table.bind_node(node.clone(), "pk-active".to_string(), near_expiry);
        assert_eq!(table.pubkey_for_node(&node), Some("pk-active".to_string()));

        // A live signaling message from this peer is processed; renew it
        // well past the (soon to lapse) original expiry.
        table.touch_node(&node, 5);

        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            table.pubkey_for_node(&node),
            Some("pk-active".to_string()),
            "touched entry must survive past its original expires_at"
        );
    }

    #[test]
    fn touch_node_is_a_noop_for_unbound_node() {
        let mut table = DiscoveryTable::default();
        let unbound = NodeId("stranger".to_string());
        // Should not panic and should not create a binding.
        table.touch_node(&unbound, 600);
        assert_eq!(table.pubkey_for_node(&unbound), None);
    }

    #[test]
    fn silent_peer_entry_still_expires_without_touch() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-silent".to_string());
        let near_expiry = now_unix_seconds() + 1;
        table.bind_node(node.clone(), "pk-silent".to_string(), near_expiry);
        assert_eq!(table.pubkey_for_node(&node), Some("pk-silent".to_string()));

        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            table.pubkey_for_node(&node),
            None,
            "an un-touched entry should still expire as before"
        );
    }

    #[test]
    fn responder_pubkeys_use_predecessors_on_rank_ring() {
        let mut table = DiscoveryTable::default();
        table.insert_pubkey_with_rank("a".to_string(), u64::MAX, "01".to_string());
        table.insert_pubkey_with_rank("b".to_string(), u64::MAX, "02".to_string());
        table.insert_pubkey_with_rank("c".to_string(), u64::MAX, "03".to_string());

        let responders = table.responder_pubkeys_for("d", "04", "c", "03", 2);
        assert_eq!(responders, vec!["c".to_string(), "b".to_string()]);
    }

    #[test]
    fn responder_pubkeys_wrap_for_lowest_rank_subject() {
        let mut table = DiscoveryTable::default();
        table.insert_pubkey_with_rank("a".to_string(), u64::MAX, "01".to_string());
        table.insert_pubkey_with_rank("b".to_string(), u64::MAX, "02".to_string());
        table.insert_pubkey_with_rank("c".to_string(), u64::MAX, "03".to_string());

        let responders = table.responder_pubkeys_for("z", "00", "c", "03", 2);
        assert_eq!(responders, vec!["c".to_string(), "b".to_string()]);
    }

    #[test]
    fn newer_epoch_wins_rebind_even_without_allow_rebind() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-reload".to_string());
        let far_future = now_unix_seconds() + 3600;

        table
            .bind_node_with_epoch(
                node.clone(),
                "pk-old".to_string(),
                far_future,
                "rank".to_string(),
                Some(100),
                false,
            )
            .unwrap();

        let outcome = table
            .bind_node_with_epoch(
                node.clone(),
                "pk-new".to_string(),
                far_future,
                "rank".to_string(),
                Some(200),
                false,
            )
            .unwrap();

        assert_eq!(
            outcome,
            BindOutcome::Rebound {
                previous_pubkey: "pk-old".to_string()
            }
        );
        assert_eq!(table.pubkey_for_node(&node), Some("pk-new".to_string()));
        assert!(!table.active_pubkeys().contains(&"pk-old".to_string()));
    }

    #[test]
    fn equal_epoch_is_rejected_without_allow_rebind() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-reload".to_string());
        let far_future = now_unix_seconds() + 3600;

        table
            .bind_node_with_epoch(
                node.clone(),
                "pk-old".to_string(),
                far_future,
                "rank".to_string(),
                Some(100),
                false,
            )
            .unwrap();

        let result = table.bind_node_with_epoch(
            node,
            "pk-new".to_string(),
            far_future,
            "rank".to_string(),
            Some(100),
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn older_epoch_is_rejected_without_allow_rebind() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-reload".to_string());
        let far_future = now_unix_seconds() + 3600;

        table
            .bind_node_with_epoch(
                node.clone(),
                "pk-old".to_string(),
                far_future,
                "rank".to_string(),
                Some(100),
                false,
            )
            .unwrap();

        let result = table.bind_node_with_epoch(
            node,
            "pk-new".to_string(),
            far_future,
            "rank".to_string(),
            Some(50),
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn missing_sender_epoch_is_rejected_without_allow_rebind() {
        // Regression guard: a legacy peer that never supplies a `joined_at`
        // must keep hitting the old strict rejection, not slip through.
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-legacy".to_string());
        let far_future = now_unix_seconds() + 3600;

        table
            .bind_node_with_epoch(
                node.clone(),
                "pk-old".to_string(),
                far_future,
                "rank".to_string(),
                Some(100),
                false,
            )
            .unwrap();

        let result = table.bind_node_with_epoch(
            node,
            "pk-new".to_string(),
            far_future,
            "rank".to_string(),
            None,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn unset_stored_epoch_allows_a_later_epoch_to_rebind() {
        // A peer that upgraded mid-session (bound without an epoch, e.g. via
        // a plain `bind_node`) must not be permanently locked out just
        // because its original binding predates epoch support.
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-upgraded".to_string());
        let far_future = now_unix_seconds() + 3600;

        table.bind_node(node.clone(), "pk-old".to_string(), far_future);
        assert_eq!(table.epoch_for_node(&node), None);

        let outcome = table
            .bind_node_with_epoch(
                node.clone(),
                "pk-new".to_string(),
                far_future,
                "rank".to_string(),
                Some(1),
                false,
            )
            .unwrap();

        assert_eq!(
            outcome,
            BindOutcome::Rebound {
                previous_pubkey: "pk-old".to_string()
            }
        );
        assert_eq!(table.pubkey_for_node(&node), Some("pk-new".to_string()));
    }

    #[test]
    fn same_pubkey_with_increasing_epochs_stays_known_and_tracks_newest() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-steady".to_string());
        let far_future = now_unix_seconds() + 3600;

        let first = table
            .bind_node_with_epoch(
                node.clone(),
                "pk-steady".to_string(),
                far_future,
                "rank".to_string(),
                Some(10),
                false,
            )
            .unwrap();
        assert_eq!(first, BindOutcome::New);

        let second = table
            .bind_node_with_epoch(
                node.clone(),
                "pk-steady".to_string(),
                far_future,
                "rank".to_string(),
                Some(20),
                false,
            )
            .unwrap();
        assert_eq!(second, BindOutcome::Known);
        assert_eq!(table.epoch_for_node(&node), Some(20));
    }

    #[test]
    fn unbind_node_removes_binding_and_pubkey_entry() {
        let mut table = DiscoveryTable::default();
        let node = NodeId("peer-leaving".to_string());
        let far_future = now_unix_seconds() + 3600;
        table.bind_node(node.clone(), "pk-leaving".to_string(), far_future);

        let removed = table.unbind_node(&node);
        assert_eq!(removed, Some("pk-leaving".to_string()));
        assert_eq!(table.pubkey_for_node(&node), None);
        assert!(!table.active_pubkeys().contains(&"pk-leaving".to_string()));
    }
}
