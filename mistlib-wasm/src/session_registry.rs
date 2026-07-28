//! Pure bookkeeping for the multi-room session map: which rooms are
//! currently joined, in what order they were joined, and lookup/insert/
//! remove for the per-room state (`T`). Deliberately generic and free of
//! any wasm/browser types so it can be unit-tested on the host target --
//! see `mistlib-wasm/tests/session_registry.rs`, which follows the same
//! `#[path]` pattern as `transport/webrtc/offer_guard.rs` to work around
//! this crate's `#![cfg(target_arch = "wasm32")]` gate (tests live only in
//! `tests/`, not inline here, matching the rest of this crate).

use std::collections::HashMap;

pub struct SessionRegistry<T> {
    sessions: HashMap<String, T>,
    join_order: Vec<String>,
}

impl<T> Default for SessionRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SessionRegistry<T> {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            join_order: Vec::new(),
        }
    }

    pub fn contains(&self, room_id: &str) -> bool {
        self.sessions.contains_key(room_id)
    }

    pub fn get(&self, room_id: &str) -> Option<&T> {
        self.sessions.get(room_id)
    }

    /// Inserts the session for `room_id`, appending it to the join order the
    /// first time this room is seen. Callers implementing the multi-room
    /// join contract ("already active -> re-announce, not active -> build a
    /// new session") should check `contains` before building `T`, so in
    /// practice this never overwrites an existing entry -- but if it does,
    /// the old value is returned and the room's join-order position is left
    /// unchanged.
    pub fn insert(&mut self, room_id: String, session: T) -> Option<T> {
        if !self.sessions.contains_key(&room_id) {
            self.join_order.push(room_id.clone());
        }
        self.sessions.insert(room_id, session)
    }

    pub fn remove(&mut self, room_id: &str) -> Option<T> {
        self.join_order.retain(|id| id != room_id);
        self.sessions.remove(room_id)
    }

    /// Removes and returns every session, oldest-joined first, leaving the
    /// registry empty.
    pub fn drain_all(&mut self) -> Vec<(String, T)> {
        let order = std::mem::take(&mut self.join_order);
        order
            .into_iter()
            .filter_map(|id| self.sessions.remove(&id).map(|s| (id, s)))
            .collect()
    }

    /// The first-joined session still present -- the fallback target for
    /// operations that need "some" session when none is a better match.
    pub fn first(&self) -> Option<(&String, &T)> {
        self.join_order
            .first()
            .and_then(|id| self.sessions.get(id).map(|s| (id, s)))
    }

    /// Iterates sessions in join order (oldest joined first).
    pub fn iter_in_join_order(&self) -> impl Iterator<Item = (&String, &T)> {
        self.join_order
            .iter()
            .filter_map(move |id| self.sessions.get(id).map(|s| (id, s)))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}
