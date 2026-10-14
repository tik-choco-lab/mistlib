// Pure WASM guards share one host test binary; each module keeps its own cases.
#[path = "guards/backpressure.rs"]
mod backpressure;
#[path = "guards/candidate_delivery.rs"]
mod candidate_delivery;
#[path = "guards/ice_config.rs"]
mod ice_config;
#[path = "guards/ice_restart.rs"]
mod ice_restart;
#[path = "guards/isolation.rs"]
mod isolation;
#[path = "guards/message_guard.rs"]
mod message_guard;
#[path = "guards/negotiation_delivery.rs"]
mod negotiation_delivery;
#[path = "guards/offer_failure.rs"]
mod offer_failure;
#[path = "guards/offer_guard.rs"]
mod offer_guard;
#[path = "guards/pending_candidates.rs"]
mod pending_candidates;
#[path = "guards/recovery.rs"]
mod recovery;
#[path = "guards/request_guard.rs"]
mod request_guard;
#[path = "guards/sctp_limit.rs"]
mod sctp_limit;
#[path = "guards/sdp_lines.rs"]
mod sdp_lines;
#[path = "guards/send_queue.rs"]
mod send_queue;
#[path = "guards/session_registry.rs"]
mod session_registry;
