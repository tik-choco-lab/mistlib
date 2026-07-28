use serde::{Deserialize, Serialize};

mod url;

use self::url::{is_local_http_url, is_local_relay, is_supported_http_url};

const DEFAULT_NOSTR_INVITE_SALT: &str = "nostr-sig-test-local-salt";
const DEFAULT_NOSTR_INVITE_CODE: &str = "dev-invite-001";
const DEFAULT_NOSTR_DISCOVERY_KIND: u32 = 25049;
const DEFAULT_NOSTR_MESSAGE_KIND: u32 = 25050;
const DEFAULT_NOSTR_TTL_SECONDS: u64 = 600;
const DEFAULT_NOSTR_MAX_CLOCK_SKEW_SECONDS: u64 = 300;

fn default_discovery_kind() -> u32 {
    DEFAULT_NOSTR_DISCOVERY_KIND
}
fn default_message_kind() -> u32 {
    DEFAULT_NOSTR_MESSAGE_KIND
}
fn default_ttl_seconds() -> u64 {
    DEFAULT_NOSTR_TTL_SECONDS
}
fn default_max_clock_skew_seconds() -> u64 {
    DEFAULT_NOSTR_MAX_CLOCK_SKEW_SECONDS
}
fn default_invite_salt() -> String {
    DEFAULT_NOSTR_INVITE_SALT.to_string()
}
fn default_invite_code() -> String {
    DEFAULT_NOSTR_INVITE_CODE.to_string()
}
pub const DEFAULT_NOSTR_RELAY_LIST_URL: &str = "https://data.tik-choco.com/server/relays.json";

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SignalingMode {
    WebSocket,
    #[default]
    Nostr,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SignalingConfig {
    #[serde(default)]
    pub mode: SignalingMode,
    #[serde(default)]
    pub nostr: Option<NostrSignalingConfig>,
}

impl Default for SignalingConfig {
    fn default() -> Self {
        Self::nostr()
    }
}

impl SignalingConfig {
    pub fn websocket() -> Self {
        Self {
            mode: SignalingMode::WebSocket,
            nostr: None,
        }
    }

    pub fn nostr() -> Self {
        Self {
            mode: SignalingMode::Nostr,
            nostr: Some(NostrSignalingConfig::default()),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NostrSignalingConfig {
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_list_url: Option<String>,
    #[serde(default = "default_discovery_kind")]
    pub discovery_kind: u32,
    #[serde(default = "default_message_kind")]
    pub message_kind: u32,
    #[serde(default = "default_ttl_seconds")]
    pub ttl_seconds: u64,
    /// 受信イベントの `created_at` / `expiration` 検証で許容する時計ずれ秒数。
    ///
    /// 時刻同期できない環境（共用計算機など）でノードの時計が数分ずれていても
    /// 相互発見できるようにするためのローカル検証パラメータ。ワイヤーフォーマット
    /// には影響しないため、ピア同士で異なる値を設定していても相互通信は可能
    /// （この値は自ノードが「何を受理するか」だけを決める）。値を大きくすると
    /// リプレイ受容窓（`ttl_seconds + max_clock_skew_seconds`）が広がるトレードオフがある。
    #[serde(default = "default_max_clock_skew_seconds")]
    pub max_clock_skew_seconds: u64,
    #[serde(default = "default_invite_salt")]
    pub invite_salt: String,
    #[serde(default = "default_invite_code")]
    pub invite_code: String,
}

impl Default for NostrSignalingConfig {
    fn default() -> Self {
        Self {
            relays: vec![],
            relay_list_url: None,
            discovery_kind: 25049,
            message_kind: 25050,
            ttl_seconds: 600,
            max_clock_skew_seconds: DEFAULT_NOSTR_MAX_CLOCK_SKEW_SECONDS,
            invite_salt: DEFAULT_NOSTR_INVITE_SALT.to_string(),
            invite_code: DEFAULT_NOSTR_INVITE_CODE.to_string(),
        }
    }
}

impl NostrSignalingConfig {
    pub fn effective_relay_list_url(&self) -> Option<&str> {
        match self.relay_list_url.as_deref() {
            Some(url) => Some(url),
            None if self.relays.is_empty() => Some(DEFAULT_NOSTR_RELAY_LIST_URL),
            None => None,
        }
    }

    pub fn validate(&self) -> bool {
        self.has_relay_source()
            && self.relays.iter().all(|relay| !relay.trim().is_empty())
            && self.explicit_relay_list_url_is_valid()
            && self.discovery_kind != self.message_kind
            && self.ttl_seconds > 0
            && !self.invite_salt.is_empty()
            && !self.invite_code.is_empty()
            && (!self.uses_default_invite() || !self.uses_explicit_public_relay_sources())
    }

    fn has_inline_relays(&self) -> bool {
        !self.relays.is_empty()
    }

    fn has_relay_source(&self) -> bool {
        self.has_inline_relays()
            || self.has_supported_relay_list_url()
            || self.uses_implicit_default_relay_list_url()
    }

    fn has_supported_relay_list_url(&self) -> bool {
        self.relay_list_url
            .as_deref()
            .is_some_and(is_supported_http_url)
    }

    fn explicit_relay_list_url_is_valid(&self) -> bool {
        self.relay_list_url
            .as_deref()
            .is_none_or(is_supported_http_url)
    }

    fn uses_implicit_default_relay_list_url(&self) -> bool {
        self.relays.is_empty() && self.relay_list_url.is_none()
    }

    fn uses_explicit_public_relay_sources(&self) -> bool {
        self.relays.iter().any(|relay| !is_local_relay(relay))
            || self
                .relay_list_url
                .as_deref()
                .is_some_and(|url| !is_local_http_url(url))
    }

    fn uses_default_invite(&self) -> bool {
        self.invite_salt == DEFAULT_NOSTR_INVITE_SALT
            && self.invite_code == DEFAULT_NOSTR_INVITE_CODE
    }
}
