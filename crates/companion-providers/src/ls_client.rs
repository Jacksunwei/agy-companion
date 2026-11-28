use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use bytes::{Buf, BytesMut};
use companion_core::{CoreError, CsrfToken, LsEndpoint, LsHost};
use http::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
};
use serde::de::{self, DeserializeOwned, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const ANTIGRAVITY_LS_ADDRESS_ENV_VAR: &str = "ANTIGRAVITY_LS_ADDRESS";
pub const ANTIGRAVITY_CSRF_TOKEN_ENV_VAR: &str = "ANTIGRAVITY_CSRF_TOKEN";

pub const HEADER_ANTIGRAVITY_CSRF_TOKEN: &str = "x-antigravity-csrf-token";
pub const HEADER_CODEIUM_CSRF_TOKEN: &str = "x-codeium-csrf-token";
pub const HEADER_CONNECT_PROTOCOL_VERSION: &str = "connect-protocol-version";
pub const HEADER_CONNECT_ACCEPT_ENCODING: &str = "connect-accept-encoding";
pub const HEADER_CONNECT_CONTENT_ENCODING: &str = "connect-content-encoding";

pub const DEFAULT_LS_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_LS_UNARY_TIMEOUT: Duration = Duration::from_secs(5);

pub const MAX_LS_MODEL_CONFIG_BYTES: usize = 512 * 1024;
pub const MAX_LS_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// `CLIENT_TRAJECTORY_VERBOSITY_PROD_UI` enum value in `jetski_cortex_pb`.
pub const TRAJECTORY_VERBOSITY_PROD_UI: u32 = 2;

const RPC_GET_CASCADE_MODEL_CONFIG_DATA: &str =
    "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData";
const RPC_GET_CASCADE_TRAJECTORY_STEPS: &str =
    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps";
const RPC_GET_CASCADE_TRAJECTORY_GENERATOR_METADATA: &str =
    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata";
const RPC_GET_ALL_CASCADE_TRAJECTORIES: &str =
    "/exa.language_server_pb.LanguageServerService/GetAllCascadeTrajectories";
const RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES: &str =
    "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries";

const CONNECT_STREAM_EMPTY_JSON_ENVELOPE: [u8; 7] = [0x00, 0x00, 0x00, 0x00, 0x02, b'{', b'}'];
const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
const CONNECT_FLAG_END_STREAM: u8 = 0x02;
const CONNECT_ENVELOPE_HEADER_BYTES: usize = 5;

const FAMILY_UNRESOLVED: u8 = 0;
const FAMILY_IPV4: u8 = 1;
const FAMILY_IPV6: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LsError {
    #[error("invalid Antigravity LS endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("invalid Antigravity CSRF token")]
    InvalidToken,
    #[error("failed to build loopback Antigravity LS HTTP client")]
    ClientBuild,
    #[error("Antigravity LS {rpc} request timed out")]
    Timeout { rpc: &'static str },
    #[error("Antigravity LS {rpc} unreachable (connection failed)")]
    Connect { rpc: &'static str },
    #[error("Antigravity LS {rpc} redirect rejected")]
    Redirect { rpc: &'static str },
    #[error("Antigravity LS {rpc} transport error")]
    Transport { rpc: &'static str },
    #[error("Antigravity LS {rpc} unauthenticated")]
    Unauthenticated { rpc: &'static str },
    #[error("Antigravity LS {rpc} permission denied")]
    PermissionDenied { rpc: &'static str },
    #[error("Antigravity LS {rpc} unimplemented")]
    Unimplemented { rpc: &'static str },
    #[error("Antigravity LS {rpc} returned HTTP {status}")]
    HttpStatus { rpc: &'static str, status: u16 },
    #[error("Antigravity LS {rpc} returned Connect error '{code}'")]
    ConnectError { rpc: &'static str, code: String },
    #[error("Antigravity LS {rpc} returned unsupported compressed encoding")]
    UnsupportedCompression { rpc: &'static str },
    #[error("Antigravity LS {rpc} response exceeded {max_bytes}-byte size limit")]
    ResponseTooLarge { rpc: &'static str, max_bytes: usize },
    #[error("Antigravity LS {rpc} returned invalid JSON")]
    InvalidJson { rpc: &'static str },
    #[error("Antigravity LS {rpc} returned malformed Connect frame")]
    InvalidFrame { rpc: &'static str },
}

// ---------------------------------------------------------------------------
// Custom proto3 JSON deserializers (string OR numeric integers, zero Value)
// ---------------------------------------------------------------------------

fn deserialize_u64_from_str_or_num<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct U64Visitor;

    impl<'de> Visitor<'de> for U64Visitor {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a u64, integer, numeric string, or null")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(if v < 0 { 0 } else { v as u64 })
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(0);
            }
            let parsed = trimmed
                .parse::<i128>()
                .map_err(|_| E::custom("invalid integer string"))?;
            if parsed <= 0 {
                Ok(0)
            } else if parsed > i128::from(u64::MAX) {
                Ok(u64::MAX)
            } else {
                Ok(parsed as u64)
            }
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(0)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(0)
        }
    }

    deserializer.deserialize_any(U64Visitor)
}

fn deserialize_u32_from_str_or_num<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let val = deserialize_u64_from_str_or_num(deserializer)?;
    Ok(u32::try_from(val).unwrap_or(u32::MAX))
}

fn deserialize_vec_u32_from_str_or_num<'de, D>(deserializer: D) -> Result<Vec<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    struct U32Item(u32);
    impl<'de> Deserialize<'de> for U32Item {
        fn deserialize<D2: Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
            deserialize_u32_from_str_or_num(d).map(Self)
        }
    }

    struct VecU32Visitor;
    impl<'de> Visitor<'de> for VecU32Visitor {
        type Value = Vec<u32>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a sequence of u32 or numeric strings, or null")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(U32Item(v)) = seq.next_element()? {
                out.push(v);
            }
            Ok(out)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(VecU32Visitor)
}

fn deserialize_i32_from_str_or_num<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    struct I32Visitor;

    impl<'de> Visitor<'de> for I32Visitor {
        type Value = i32;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an i32, integer, numeric string, or null")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(i32::try_from(v).unwrap_or(i32::MAX))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(0);
            }
            let parsed = trimmed
                .parse::<i64>()
                .map_err(|_| E::custom("invalid i32 string"))?;
            Ok(parsed.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(0)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(0)
        }
    }

    deserializer.deserialize_any(I32Visitor)
}

fn deserialize_opt_string_or_i64<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptStringOrI64Visitor;

    impl<'de> Visitor<'de> for OptStringOrI64Visitor {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string, integer enum value, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            if v.is_empty() {
                Ok(None)
            } else {
                Ok(Some(v.to_string()))
            }
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            if v.is_empty() { Ok(None) } else { Ok(Some(v)) }
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(self)
        }
    }

    deserializer.deserialize_any(OptStringOrI64Visitor)
}

// ---------------------------------------------------------------------------
// Strict Allowlist Serde DTOs (ZERO serde_json::Value, NO deny_unknown_fields)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsModelOrAlias {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsContextWindowOption {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expensive: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsModelConfig {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub model_or_alias: Option<LsModelOrAlias>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub provider: Option<String>,
    #[serde(default)]
    pub model_url: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub context_window_options: Vec<LsContextWindowOption>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetCascadeModelConfigDataResponse {
    #[serde(default)]
    client_model_configs: Vec<LsModelConfig>,
    #[serde(default)]
    cached_cascade_model_configs: Vec<LsModelConfig>,
    #[serde(default)]
    battle_mode_model_configs: Vec<LsModelConfig>,
}

/// Strictly allowlisted 3-way attribution fingerprint from `ModelUsageStats.response_header`.
///
/// Captures ONLY `sessionID`, `sessionId`, and `responseId` — all other response headers are
/// discarded during deserialization without heap allocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LsResponseHeaderFingerprint {
    #[serde(default, rename = "sessionID")]
    pub session_id_upper: Option<String>,
    #[serde(default, rename = "sessionId")]
    pub session_id_lower: Option<String>,
    #[serde(default, rename = "responseId")]
    pub response_id: Option<String>,
}

impl LsResponseHeaderFingerprint {
    /// True when `sessionID` (capital `ID`) is present and non-empty (`CodeAssistClient` built-in).
    pub fn has_builtin_session_id(&self) -> bool {
        self.session_id_upper
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }

    /// True when `sessionId` (lowercase `d`) is present and non-empty (`genaisdk` direct AI Studio).
    pub fn has_direct_session_id(&self) -> bool {
        self.session_id_lower
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }

    /// True when `responseId` is present and non-empty in the response header map.
    pub fn has_response_id(&self) -> bool {
        self.response_id.as_deref().is_some_and(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsModelUsage {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub input_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub output_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub cache_read_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub cache_write_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub response_output_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub thinking_output_tokens: u64,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub api_provider: Option<String>,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub provider_assigned_message_id: Option<String>,
    #[serde(default)]
    pub response_header: Option<LsResponseHeaderFingerprint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsRetryInfo {
    #[serde(default)]
    pub usage: Option<LsModelUsage>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub retry_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsModelInfo {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub model_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub api_provider: Option<String>,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub vertex_model_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub model_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsStepMetadata {
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub viewable_at: Option<String>,
    #[serde(default)]
    pub finished_generating_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub generator_model: Option<String>,
    #[serde(default)]
    pub model_usage: Option<LsModelUsage>,
    #[serde(default)]
    pub retry_infos: Vec<LsRetryInfo>,
    #[serde(default)]
    pub model_info: Option<LsModelInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsTrajectoryStep {
    #[serde(
        default,
        rename = "type",
        alias = "stepType",
        deserialize_with = "deserialize_opt_string_or_i64"
    )]
    pub step_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub status: Option<String>,
    #[serde(default)]
    pub metadata: Option<LsStepMetadata>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetCascadeTrajectoryStepsRequest<'a> {
    cascade_id: &'a str,
    step_offset: u32,
    trajectory_verbosity: u32,
    disable_rehydration: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetCascadeTrajectoryStepsResponse {
    #[serde(default)]
    steps: Vec<LsTrajectoryStep>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsTrajectoryMetadata {
    #[serde(default)]
    pub parent_conversation_id: Option<String>,
    #[serde(default)]
    pub root_conversation_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i32_from_str_or_num")]
    pub nesting_depth: i32,
}

/// Strictly allowlisted `CascadeTrajectorySummary` DTO (omits `summary` text and `workspaces`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsTrajectorySummary {
    #[serde(default)]
    pub trajectory_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub trajectory_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_u32_from_str_or_num")]
    pub step_count: u32,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub source: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub last_modified_time: Option<String>,
    #[serde(default)]
    pub trajectory_metadata: Option<LsTrajectoryMetadata>,
}

/// Strict allowlist DTO for `ContextWindowMetadata` inside generator metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsContextWindowMetadata {
    #[serde(default, deserialize_with = "deserialize_u64_from_str_or_num")]
    pub max_context_tokens: u64,
}

/// Strict allowlist DTO for `ChatStartMetadata` inside generator metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsChatStartMetadata {
    #[serde(default)]
    pub context_window_metadata: Option<LsContextWindowMetadata>,
}

/// Strict allowlist DTO for `ChatModelMetadata` inside generator metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsChatModelMetadata {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<LsModelUsage>,
    #[serde(default)]
    pub response_model: Option<String>,
    #[serde(default)]
    pub model_display_name: Option<String>,
    #[serde(default)]
    pub chat_start_metadata: Option<LsChatStartMetadata>,
}

/// Strict allowlist DTO for a single `GeneratorMetadata` entry returned by
/// `GetCascadeTrajectoryGeneratorMetadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsGeneratorMetadata {
    #[serde(default)]
    pub chat_model: Option<LsChatModelMetadata>,
    #[serde(default, deserialize_with = "deserialize_vec_u32_from_str_or_num")]
    pub step_indices: Vec<u32>,
}

impl LsGeneratorMetadata {
    /// Returns the authoritative compaction window (`maxContextTokens`) when present and `> 0`.
    pub fn max_context_tokens(&self) -> Option<u64> {
        self.chat_model
            .as_ref()
            .and_then(|chat_model| chat_model.chat_start_metadata.as_ref())
            .and_then(|start| start.context_window_metadata.as_ref())
            .map(|window| window.max_context_tokens)
            .filter(|&tokens| tokens > 0)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetCascadeTrajectoryGeneratorMetadataRequest<'a> {
    cascade_id: &'a str,
    generator_metadata_offset: u32,
    include_messages: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetCascadeTrajectoryGeneratorMetadataResponse {
    #[serde(default)]
    generator_metadata: Vec<LsGeneratorMetadata>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetAllCascadeTrajectoriesRequest {
    exclude_subtrajectories: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAllCascadeTrajectoriesResponse {
    #[serde(default)]
    trajectory_summaries: HashMap<String, LsTrajectorySummary>,
}

/// Decoded `JetboxSubscribeToSummariesResponse` frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LsSummaryDelta {
    #[serde(default)]
    pub updates: HashMap<String, LsTrajectorySummary>,
    #[serde(default)]
    pub deletes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ConnectEndStream {
    #[serde(default)]
    error: Option<ConnectEndStreamError>,
}

#[derive(Debug, Default, Deserialize)]
struct ConnectEndStreamError {
    #[serde(default)]
    code: Option<String>,
}

// ---------------------------------------------------------------------------
// Loopback-Only Antigravity LS RPC Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LsClient {
    endpoint: LsEndpoint,
    token: CsrfToken,
    unary_client: reqwest::Client,
    stream_client: reqwest::Client,
    resolved_family: Arc<AtomicU8>,
}

impl fmt::Debug for LsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LsClient")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("resolved_family", &self.resolved_family_host())
            .finish_non_exhaustive()
    }
}

impl LsClient {
    /// Builds a loopback-hardened Antigravity LS client using the default timeouts
    /// (2s connect timeout, 5s unary request timeout, unlimited streaming lifetime).
    pub fn new(endpoint: LsEndpoint, token: CsrfToken) -> Result<Self, LsError> {
        Self::with_timeouts(
            endpoint,
            token,
            DEFAULT_LS_CONNECT_TIMEOUT,
            DEFAULT_LS_UNARY_TIMEOUT,
        )
    }

    /// Builds a loopback-hardened Antigravity LS client with custom connect and unary timeouts.
    pub fn with_timeouts(
        endpoint: LsEndpoint,
        token: CsrfToken,
        connect_timeout: Duration,
        unary_timeout: Duration,
    ) -> Result<Self, LsError> {
        let unary_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .timeout(unary_timeout)
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| LsError::ClientBuild)?;

        let stream_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| LsError::ClientBuild)?;

        let initial_family = match endpoint.host() {
            LsHost::Ipv4 => FAMILY_IPV4,
            LsHost::Ipv6 => FAMILY_IPV6,
            LsHost::Localhost => FAMILY_UNRESOLVED,
        };

        Ok(Self {
            endpoint,
            token,
            unary_client,
            stream_client,
            resolved_family: Arc::new(AtomicU8::new(initial_family)),
        })
    }

    /// Constructs an `LsClient` from `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
    ///
    /// Returns `Ok(None)` cleanly when either environment variable is absent or empty
    /// (e.g., standalone `agy` CLI or detached `:24141` daemon).
    pub fn from_env() -> Result<Option<Self>, LsError> {
        let raw_addr = std::env::var(ANTIGRAVITY_LS_ADDRESS_ENV_VAR).ok();
        let raw_token = std::env::var(ANTIGRAVITY_CSRF_TOKEN_ENV_VAR).ok();
        Self::from_env_values(raw_addr.as_deref(), raw_token.as_deref())
    }

    /// Pure helper mirroring [`Self::from_env`] for hermetic testing without mutating process env.
    pub fn from_env_values(
        address: Option<&str>,
        token: Option<&str>,
    ) -> Result<Option<Self>, LsError> {
        let (Some(raw_addr), Some(raw_token)) = (address, token) else {
            return Ok(None);
        };
        if raw_addr.is_empty() || raw_token.is_empty() {
            return Ok(None);
        }

        let endpoint = LsEndpoint::parse(raw_addr).map_err(|err| match err {
            CoreError::InvalidLsEndpoint { reason } => LsError::InvalidEndpoint(reason),
            other => LsError::InvalidEndpoint(other.to_string()),
        })?;
        let csrf_token = CsrfToken::parse(raw_token).map_err(|_| LsError::InvalidToken)?;
        Self::new(endpoint, csrf_token).map(Some)
    }

    pub fn endpoint(&self) -> &LsEndpoint {
        &self.endpoint
    }

    /// Returns the latched literal loopback IP family (`Ipv4` or `Ipv6`), or `None` if an
    /// `LsHost::Localhost` endpoint has not yet completed a request.
    pub fn resolved_family_host(&self) -> Option<LsHost> {
        match self.resolved_family.load(Ordering::Acquire) {
            FAMILY_IPV4 => Some(LsHost::Ipv4),
            FAMILY_IPV6 => Some(LsHost::Ipv6),
            _ => None,
        }
    }

    pub async fn get_cascade_model_config_data(&self) -> Result<Vec<LsModelConfig>, LsError> {
        let resp: GetCascadeModelConfigDataResponse = self
            .post_unary_json(
                "GetCascadeModelConfigData",
                RPC_GET_CASCADE_MODEL_CONFIG_DATA,
                b"{}".to_vec(),
                MAX_LS_MODEL_CONFIG_BYTES,
            )
            .await?;
        let total_cap = resp
            .client_model_configs
            .len()
            .saturating_add(resp.cached_cascade_model_configs.len())
            .saturating_add(resp.battle_mode_model_configs.len());
        let mut merged: Vec<LsModelConfig> = Vec::with_capacity(total_cap);
        let mut key_to_idx: HashMap<String, usize> = HashMap::with_capacity(total_cap);
        let normalize_key = |raw: &str| {
            let trimmed = raw.trim();
            companion_core::normalize_placeholder_enum_key(trimmed)
                .unwrap_or_else(|| trimmed.to_string())
        };
        let mut push_or_merge = |configs: Vec<LsModelConfig>| {
            for cfg in configs {
                let model_key = cfg
                    .model_or_alias
                    .as_ref()
                    .and_then(|m| m.model.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(normalize_key);
                let alias_key = cfg
                    .model_or_alias
                    .as_ref()
                    .and_then(|m| m.alias.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(normalize_key);
                let existing_idx = if let Some(ref k) = model_key {
                    key_to_idx.get(k).copied()
                } else {
                    alias_key
                        .as_deref()
                        .and_then(|k| key_to_idx.get(k).copied())
                };
                if let Some(idx) = existing_idx {
                    merge_ls_model_config(&mut merged[idx], cfg);
                    if let Some(k) = model_key {
                        key_to_idx.entry(k).or_insert(idx);
                    }
                    if let Some(k) = alias_key {
                        key_to_idx.entry(k).or_insert(idx);
                    }
                } else {
                    let idx = merged.len();
                    if let Some(k) = model_key {
                        key_to_idx.entry(k).or_insert(idx);
                    }
                    if let Some(k) = alias_key {
                        key_to_idx.entry(k).or_insert(idx);
                    }
                    merged.push(cfg);
                }
            }
        };
        // clientModelConfigs takes precedence over cached and battle-mode configs,
        // while backfilling any missing/empty fields from lower-precedence sections.
        push_or_merge(resp.client_model_configs);
        push_or_merge(resp.cached_cascade_model_configs);
        push_or_merge(resp.battle_mode_model_configs);
        Ok(merged)
    }

    pub async fn get_cascade_trajectory_steps(
        &self,
        cascade_id: &str,
        step_offset: u32,
    ) -> Result<Vec<LsTrajectoryStep>, LsError> {
        let req_body = serde_json::to_vec(&GetCascadeTrajectoryStepsRequest {
            cascade_id,
            step_offset,
            trajectory_verbosity: TRAJECTORY_VERBOSITY_PROD_UI,
            disable_rehydration: true,
        })
        .map_err(|_| LsError::InvalidJson {
            rpc: "GetCascadeTrajectorySteps",
        })?;

        let resp: GetCascadeTrajectoryStepsResponse = self
            .post_unary_json(
                "GetCascadeTrajectorySteps",
                RPC_GET_CASCADE_TRAJECTORY_STEPS,
                req_body,
                MAX_LS_RESPONSE_BYTES,
            )
            .await?;
        Ok(resp.steps)
    }

    pub async fn get_cascade_trajectory_generator_metadata(
        &self,
        cascade_id: &str,
        generator_metadata_offset: u32,
    ) -> Result<Vec<LsGeneratorMetadata>, LsError> {
        let req_body = serde_json::to_vec(&GetCascadeTrajectoryGeneratorMetadataRequest {
            cascade_id,
            generator_metadata_offset,
            include_messages: false,
        })
        .map_err(|_| LsError::InvalidJson {
            rpc: "GetCascadeTrajectoryGeneratorMetadata",
        })?;

        let resp: GetCascadeTrajectoryGeneratorMetadataResponse = self
            .post_unary_json(
                "GetCascadeTrajectoryGeneratorMetadata",
                RPC_GET_CASCADE_TRAJECTORY_GENERATOR_METADATA,
                req_body,
                MAX_LS_RESPONSE_BYTES,
            )
            .await?;
        Ok(resp.generator_metadata)
    }

    pub async fn get_all_cascade_trajectories(
        &self,
        exclude_subtrajectories: bool,
    ) -> Result<HashMap<String, LsTrajectorySummary>, LsError> {
        let req_body = serde_json::to_vec(&GetAllCascadeTrajectoriesRequest {
            exclude_subtrajectories,
        })
        .map_err(|_| LsError::InvalidJson {
            rpc: "GetAllCascadeTrajectories",
        })?;

        let resp: GetAllCascadeTrajectoriesResponse = self
            .post_unary_json(
                "GetAllCascadeTrajectories",
                RPC_GET_ALL_CASCADE_TRAJECTORIES,
                req_body,
                MAX_LS_RESPONSE_BYTES,
            )
            .await?;
        Ok(resp.trajectory_summaries)
    }

    pub async fn subscribe_to_summaries(&self) -> Result<LsSummaryStream, LsError> {
        let rpc = "JetboxSubscribeToSummaries";
        let mut headers = self.build_common_headers()?;
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/connect+json"),
        );
        headers.insert(
            HeaderName::from_static(HEADER_CONNECT_ACCEPT_ENCODING),
            HeaderValue::from_static("identity"),
        );

        let resp = self
            .send_with_loopback_latch(
                &self.stream_client,
                rpc,
                RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES,
                headers,
                CONNECT_STREAM_EMPTY_JSON_ENVELOPE.to_vec(),
            )
            .await?;
        validate_ls_response_headers(rpc, &resp)?;

        Ok(LsSummaryStream {
            resp,
            buf: BytesMut::new(),
            eof: false,
            rpc,
        })
    }

    fn build_common_headers(&self) -> Result<HeaderMap, LsError> {
        let mut headers = HeaderMap::with_capacity(6);
        let mut csrf_val =
            HeaderValue::from_str(self.token.expose()).map_err(|_| LsError::InvalidToken)?;
        csrf_val.set_sensitive(true);
        headers.insert(
            HeaderName::from_static(HEADER_ANTIGRAVITY_CSRF_TOKEN),
            csrf_val.clone(),
        );
        headers.insert(HeaderName::from_static(HEADER_CODEIUM_CSRF_TOKEN), csrf_val);
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        headers.insert(
            HeaderName::from_static(HEADER_CONNECT_PROTOCOL_VERSION),
            HeaderValue::from_static("1"),
        );
        Ok(headers)
    }

    async fn send_with_loopback_latch(
        &self,
        client: &reqwest::Client,
        rpc: &'static str,
        path: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, LsError> {
        match self.resolved_family.load(Ordering::Acquire) {
            FAMILY_IPV4 => {
                let url = self.endpoint.ipv4_url(path);
                client
                    .post(&url)
                    .headers(headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(|err| sanitize_ls_transport_error(rpc, &err))
            }
            FAMILY_IPV6 => {
                let url = self.endpoint.ipv6_url(path);
                client
                    .post(&url)
                    .headers(headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(|err| sanitize_ls_transport_error(rpc, &err))
            }
            _ => {
                let ipv4_url = self.endpoint.ipv4_url(path);
                match client
                    .post(&ipv4_url)
                    .headers(headers.clone())
                    .body(body.clone())
                    .send()
                    .await
                {
                    Ok(resp) => {
                        self.resolved_family.store(FAMILY_IPV4, Ordering::Release);
                        Ok(resp)
                    }
                    Err(err) if err.is_connect() => {
                        let ipv6_url = self.endpoint.ipv6_url(path);
                        let resp = client
                            .post(&ipv6_url)
                            .headers(headers)
                            .body(body)
                            .send()
                            .await
                            .map_err(|retry_err| sanitize_ls_transport_error(rpc, &retry_err))?;
                        self.resolved_family.store(FAMILY_IPV6, Ordering::Release);
                        Ok(resp)
                    }
                    Err(err) => Err(sanitize_ls_transport_error(rpc, &err)),
                }
            }
        }
    }

    async fn post_unary_json<T: DeserializeOwned>(
        &self,
        rpc: &'static str,
        path: &str,
        body: Vec<u8>,
        max_bytes: usize,
    ) -> Result<T, LsError> {
        let mut headers = self.build_common_headers()?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let mut resp = self
            .send_with_loopback_latch(&self.unary_client, rpc, path, headers, body)
            .await?;
        validate_ls_response_headers(rpc, &resp)?;

        if resp
            .content_length()
            .is_some_and(|len| len > max_bytes as u64)
        {
            return Err(LsError::ResponseTooLarge { rpc, max_bytes });
        }

        let mut buf = Vec::new();
        let mut total_bytes: usize = 0;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|err| sanitize_ls_transport_error(rpc, &err))?
        {
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > max_bytes {
                return Err(LsError::ResponseTooLarge { rpc, max_bytes });
            }
            buf.extend_from_slice(&chunk);
        }

        serde_json::from_slice::<T>(&buf).map_err(|_| LsError::InvalidJson { rpc })
    }
}

fn is_unresolved_placeholder_model_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed.starts_with("MODEL_PLACEHOLDER_")
        || trimmed.bytes().all(|b| b.is_ascii_digit())
}

fn is_missing_model_provider(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed == "0"
        || trimmed.eq_ignore_ascii_case("MODEL_PROVIDER_UNSPECIFIED")
}

fn merge_non_empty_opt_string(target: &mut Option<String>, fallback: Option<String>) {
    if target.as_deref().is_none_or(|s| s.trim().is_empty())
        && let Some(fb) = fallback
        && !fb.trim().is_empty()
    {
        *target = Some(fb);
    }
}

fn merge_ls_model_config(existing: &mut LsModelConfig, fallback: LsModelConfig) {
    merge_non_empty_opt_string(&mut existing.label, fallback.label);
    match (&mut existing.model_or_alias, fallback.model_or_alias) {
        (None, Some(fb)) => existing.model_or_alias = Some(fb),
        (Some(acc), Some(fb)) => {
            merge_non_empty_opt_string(&mut acc.model, fb.model);
            merge_non_empty_opt_string(&mut acc.alias, fb.alias);
        }
        _ => {}
    }
    if existing
        .provider
        .as_deref()
        .is_none_or(is_missing_model_provider)
        && let Some(fb) = fallback.provider
        && !is_missing_model_provider(&fb)
    {
        existing.provider = Some(fb);
    }
    merge_non_empty_opt_string(&mut existing.model_url, fallback.model_url);

    let existing_has_real_id = existing
        .model_id
        .as_deref()
        .is_some_and(|s| !is_unresolved_placeholder_model_id(s));
    if !existing_has_real_id {
        if let Some(ref fb_id) = fallback.model_id
            && !is_unresolved_placeholder_model_id(fb_id)
        {
            existing.model_id = Some(fb_id.clone());
        } else {
            merge_non_empty_opt_string(&mut existing.model_id, fallback.model_id);
        }
    }

    if existing.context_window_options.is_empty() && !fallback.context_window_options.is_empty() {
        existing.context_window_options = fallback.context_window_options;
    }
}

/// Incremental Connect HTTP/1.1 (`application/connect+json`) 5-byte envelope frame stream for
/// `JetboxSubscribeToSummaries`.
pub struct LsSummaryStream {
    resp: reqwest::Response,
    buf: BytesMut,
    eof: bool,
    rpc: &'static str,
}

impl fmt::Debug for LsSummaryStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LsSummaryStream")
            .field("rpc", &self.rpc)
            .field("buffered_bytes", &self.buf.len())
            .field("eof", &self.eof)
            .finish_non_exhaustive()
    }
}

impl LsSummaryStream {
    /// Reads the next decoded `LsSummaryDelta` frame, returning `Ok(None)` on clean EndStream or
    /// clean EOF, and failing closed on compression flags, oversized frames, or Connect errors.
    pub async fn next_delta(&mut self) -> Result<Option<LsSummaryDelta>, LsError> {
        if self.eof {
            return Ok(None);
        }

        loop {
            if self.buf.len() >= CONNECT_ENVELOPE_HEADER_BYTES {
                let flags = self.buf[0];
                let frame_len =
                    u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]])
                        as usize;

                if (flags & CONNECT_FLAG_COMPRESSED) != 0 {
                    self.eof = true;
                    return Err(LsError::UnsupportedCompression { rpc: self.rpc });
                }
                if frame_len > MAX_LS_RESPONSE_BYTES {
                    self.eof = true;
                    return Err(LsError::ResponseTooLarge {
                        rpc: self.rpc,
                        max_bytes: MAX_LS_RESPONSE_BYTES,
                    });
                }
                if (flags & !CONNECT_FLAG_END_STREAM) != 0 {
                    self.eof = true;
                    return Err(LsError::InvalidFrame { rpc: self.rpc });
                }

                let total_frame_bytes = CONNECT_ENVELOPE_HEADER_BYTES + frame_len;
                if self.buf.len() >= total_frame_bytes {
                    self.buf.advance(CONNECT_ENVELOPE_HEADER_BYTES);
                    let payload = self.buf.split_to(frame_len);

                    if (flags & CONNECT_FLAG_END_STREAM) != 0 {
                        self.eof = true;
                        let end: ConnectEndStream = serde_json::from_slice(&payload)
                            .map_err(|_| LsError::InvalidJson { rpc: self.rpc })?;
                        if let Some(err) = end.error {
                            return Err(map_connect_end_stream_error(
                                self.rpc,
                                err.code.as_deref(),
                            ));
                        }
                        return Ok(None);
                    }

                    let delta: LsSummaryDelta = serde_json::from_slice(&payload)
                        .map_err(|_| LsError::InvalidJson { rpc: self.rpc })?;
                    return Ok(Some(delta));
                }
            }

            match self.resp.chunk().await {
                Ok(Some(chunk)) => {
                    self.buf.extend_from_slice(&chunk);
                }
                Ok(None) => {
                    self.eof = true;
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    return Err(LsError::InvalidFrame { rpc: self.rpc });
                }
                Err(err) => {
                    self.eof = true;
                    return Err(sanitize_ls_transport_error(self.rpc, &err));
                }
            }
        }
    }
}

fn validate_ls_response_headers(
    rpc: &'static str,
    resp: &reqwest::Response,
) -> Result<(), LsError> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(LsError::Unauthenticated { rpc });
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(LsError::PermissionDenied { rpc });
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::NOT_IMPLEMENTED {
        return Err(LsError::Unimplemented { rpc });
    }
    if status.is_redirection() {
        return Err(LsError::Redirect { rpc });
    }
    if status != reqwest::StatusCode::OK {
        return Err(LsError::HttpStatus {
            rpc,
            status: status.as_u16(),
        });
    }

    for header_name in [
        CONTENT_ENCODING,
        HeaderName::from_static(HEADER_CONNECT_CONTENT_ENCODING),
    ] {
        for value in resp.headers().get_all(&header_name) {
            let Ok(raw) = value.to_str() else {
                return Err(LsError::UnsupportedCompression { rpc });
            };
            for token in raw.split(',') {
                let cleaned = token.trim();
                if !cleaned.is_empty() && !cleaned.eq_ignore_ascii_case("identity") {
                    return Err(LsError::UnsupportedCompression { rpc });
                }
            }
        }
    }

    Ok(())
}

fn map_connect_end_stream_error(rpc: &'static str, raw_code: Option<&str>) -> LsError {
    let sanitized: String = raw_code
        .unwrap_or("unknown")
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(48)
        .collect();
    let lower = sanitized.to_ascii_lowercase();
    match lower.as_str() {
        "unauthenticated" => LsError::Unauthenticated { rpc },
        "permission_denied" => LsError::PermissionDenied { rpc },
        "unimplemented" => LsError::Unimplemented { rpc },
        "" => LsError::ConnectError {
            rpc,
            code: "unknown".to_string(),
        },
        _ => LsError::ConnectError { rpc, code: lower },
    }
}

fn sanitize_ls_transport_error(rpc: &'static str, err: &reqwest::Error) -> LsError {
    if err.is_timeout() {
        LsError::Timeout { rpc }
    } else if err.is_connect() {
        LsError::Connect { rpc }
    } else if err.is_redirect() {
        LsError::Redirect { rpc }
    } else {
        LsError::Transport { rpc }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::routing::post;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;

    const TEST_CSRF_SECRET: &str = "csrf-secret-test-01234567-89ab-cdef";

    fn encode_connect_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    async fn spawn_ipv4_router(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind 127.0.0.1");
        let port = listener.local_addr().expect("local addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, handle)
    }

    #[test]
    fn from_env_values_returns_none_when_absent_and_validates_when_present() {
        assert!(LsClient::from_env_values(None, None).unwrap().is_none());
        assert!(
            LsClient::from_env_values(Some("localhost:64482"), None)
                .unwrap()
                .is_none()
        );
        assert!(
            LsClient::from_env_values(None, Some(TEST_CSRF_SECRET))
                .unwrap()
                .is_none()
        );
        assert!(
            LsClient::from_env_values(Some(""), Some(TEST_CSRF_SECRET))
                .unwrap()
                .is_none()
        );
        assert!(
            LsClient::from_env_values(Some("localhost:64482"), Some(""))
                .unwrap()
                .is_none()
        );

        let err_ep =
            LsClient::from_env_values(Some("https://evil.example:443"), Some(TEST_CSRF_SECRET))
                .expect_err("https endpoint must fail");
        assert!(matches!(err_ep, LsError::InvalidEndpoint(_)));
        assert!(!err_ep.to_string().contains(TEST_CSRF_SECRET));

        let err_tok = LsClient::from_env_values(Some("localhost:64482"), Some("bad\ntoken"))
            .expect_err("control char token must fail");
        assert_eq!(err_tok, LsError::InvalidToken);
        assert!(!err_tok.to_string().contains("bad"));

        let client = LsClient::from_env_values(Some("localhost:64482"), Some(TEST_CSRF_SECRET))
            .expect("valid env")
            .expect("some client");
        assert_eq!(client.endpoint().host(), LsHost::Localhost);
        assert_eq!(client.endpoint().port(), 64482);
        assert_eq!(client.resolved_family_host(), None);

        let debug = format!("{client:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(TEST_CSRF_SECRET));
    }

    #[tokio::test]
    async fn dual_csrf_headers_identity_encoding_and_redacted_errors() {
        let captured = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let captured_clone = Arc::clone(&captured);

        let (port, handle) = spawn_ipv4_router(Router::new().route(
            RPC_GET_CASCADE_MODEL_CONFIG_DATA,
            post(move |headers: HeaderMap, body: Bytes| {
                let cap = Arc::clone(&captured_clone);
                async move {
                    let mut pairs = Vec::new();
                    for (k, v) in &headers {
                        if let Ok(s) = v.to_str() {
                            pairs.push((k.as_str().to_string(), s.to_string()));
                        }
                    }
                    *cap.lock().await = pairs;
                    assert_eq!(body.as_ref(), b"{}");
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{
                            "clientModelConfigs": [
                                {
                                    "label": "Gemini 4 Hydrogen (AI Studio) (Medium)",
                                    "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M335" },
                                    "provider": "MODEL_PROVIDER_GOOGLE",
                                    "modelUrl": "gemini-api://gemini-4-hydrogen",
                                    "modelId": "companion/gemini-4-hydrogen-medium",
                                    "contextWindowOptions": [{"name": "1M", "expensive": false}],
                                    "unknownFutureField": { "nested": [1, 2, 3] }
                                }
                            ]
                        }"#,
                    )
                }
            }),
        ))
        .await;

        let endpoint = LsEndpoint::parse(&format!("localhost:{port}")).unwrap();
        let token = CsrfToken::parse(TEST_CSRF_SECRET).unwrap();
        let client = LsClient::new(endpoint, token).unwrap();

        let configs = client
            .get_cascade_model_config_data()
            .await
            .expect("model config RPC");
        assert_eq!(configs.len(), 1);
        assert_eq!(
            configs[0].model_id.as_deref(),
            Some("companion/gemini-4-hydrogen-medium")
        );
        assert_eq!(
            configs[0].model_url.as_deref(),
            Some("gemini-api://gemini-4-hydrogen")
        );
        assert_eq!(
            configs[0]
                .model_or_alias
                .as_ref()
                .and_then(|m| m.model.as_deref()),
            Some("MODEL_PLACEHOLDER_M335")
        );
        assert_eq!(client.resolved_family_host(), Some(LsHost::Ipv4));

        let seen = captured.lock().await;
        let find_hdr = |name: &str| {
            seen.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            find_hdr(HEADER_ANTIGRAVITY_CSRF_TOKEN),
            Some(TEST_CSRF_SECRET)
        );
        assert_eq!(find_hdr(HEADER_CODEIUM_CSRF_TOKEN), Some(TEST_CSRF_SECRET));
        assert_eq!(find_hdr("accept-encoding"), Some("identity"));
        assert_eq!(find_hdr(HEADER_CONNECT_PROTOCOL_VERSION), Some("1"));
        assert_eq!(find_hdr("content-type"), Some("application/json"));
        handle.abort();

        // Error status mapping (401, 403, 404, 501, 500) never echoes body or CSRF secret
        for (status, expected) in [
            (
                StatusCode::UNAUTHORIZED,
                LsError::Unauthenticated {
                    rpc: "GetCascadeModelConfigData",
                },
            ),
            (
                StatusCode::FORBIDDEN,
                LsError::PermissionDenied {
                    rpc: "GetCascadeModelConfigData",
                },
            ),
            (
                StatusCode::NOT_FOUND,
                LsError::Unimplemented {
                    rpc: "GetCascadeModelConfigData",
                },
            ),
            (
                StatusCode::NOT_IMPLEMENTED,
                LsError::Unimplemented {
                    rpc: "GetCascadeModelConfigData",
                },
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                LsError::HttpStatus {
                    rpc: "GetCascadeModelConfigData",
                    status: 500,
                },
            ),
        ] {
            let (err_port, err_handle) = spawn_ipv4_router(Router::new().route(
                RPC_GET_CASCADE_MODEL_CONFIG_DATA,
                post(move || async move { (status, "SENSITIVE_PROMPT_OR_TOKEN_BODY") }),
            ))
            .await;
            let ep = LsEndpoint::parse(&format!("127.0.0.1:{err_port}")).unwrap();
            let tok = CsrfToken::parse(TEST_CSRF_SECRET).unwrap();
            let err_client = LsClient::new(ep, tok).unwrap();
            let err = err_client
                .get_cascade_model_config_data()
                .await
                .expect_err("non-200 must error");
            assert_eq!(err, expected);
            assert!(!err.to_string().contains("SENSITIVE_PROMPT_OR_TOKEN_BODY"));
            assert!(!err.to_string().contains(TEST_CSRF_SECRET));
            assert!(!format!("{err:?}").contains(TEST_CSRF_SECRET));
            err_handle.abort();
        }
    }

    #[tokio::test]
    async fn ipv4_to_ipv6_loopback_latch_retries_once_and_latches_ipv6() {
        let Ok(v6_listener) = TcpListener::bind(("::1", 0)).await else {
            // Host environment lacks IPv6 loopback; skip cleanly without failing.
            return;
        };
        let port = v6_listener.local_addr().expect("v6 local addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let router = Router::new().route(
            RPC_GET_CASCADE_MODEL_CONFIG_DATA,
            post(move || {
                let h = Arc::clone(&hits_clone);
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"clientModelConfigs":[]}"#,
                    )
                }
            }),
        );
        let handle = tokio::spawn(async move {
            let _ = axum::serve(v6_listener, router).await;
        });

        let endpoint = LsEndpoint::parse(&format!("localhost:{port}")).unwrap();
        let token = CsrfToken::parse(TEST_CSRF_SECRET).unwrap();
        // Use 5s connect/unary budget so Windows loopback IPv4 SYN refusal (~2s) never flakes.
        let client = LsClient::with_timeouts(
            endpoint,
            token,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();

        // First call tries 127.0.0.1 -> connect refused -> retries [::1] and latches IPv6.
        let first = client
            .get_cascade_model_config_data()
            .await
            .expect("IPv6 fallback must succeed");
        assert!(first.is_empty());
        assert_eq!(client.resolved_family_host(), Some(LsHost::Ipv6));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Second call goes directly to latched [::1].
        let second = client
            .get_cascade_model_config_data()
            .await
            .expect("latched IPv6 second call");
        assert!(second.is_empty());
        assert_eq!(client.resolved_family_host(), Some(LsHost::Ipv6));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_302_redirects_and_compressed_content_encoding_fail_closed() {
        // 302 redirect MUST NOT be followed
        let target_hits = Arc::new(AtomicUsize::new(0));
        let target_hits_clone = Arc::clone(&target_hits);
        let (redir_port, redir_handle) = spawn_ipv4_router(
            Router::new()
                .route(
                    RPC_GET_CASCADE_MODEL_CONFIG_DATA,
                    post(|| async {
                        (
                            StatusCode::FOUND,
                            [(header::LOCATION, HeaderValue::from_static("/exfil"))],
                            "",
                        )
                    }),
                )
                .route(
                    "/exfil",
                    post(move || {
                        let hits = Arc::clone(&target_hits_clone);
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, r#"{"clientModelConfigs":[]}"#)
                        }
                    }),
                ),
        )
        .await;

        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{redir_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        let err = client
            .get_cascade_model_config_data()
            .await
            .expect_err("302 redirect must be rejected");
        assert_eq!(
            err,
            LsError::Redirect {
                rpc: "GetCascadeModelConfigData"
            }
        );
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
        redir_handle.abort();

        // Content-Encoding: gzip -> fail closed UnsupportedCompression
        let (gzip_port, gzip_handle) = spawn_ipv4_router(Router::new().route(
            RPC_GET_CASCADE_MODEL_CONFIG_DATA,
            post(|| async {
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::CONTENT_ENCODING, "gzip"),
                    ],
                    r#"{"clientModelConfigs":[]}"#,
                )
            }),
        ))
        .await;
        let gzip_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{gzip_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            gzip_client
                .get_cascade_model_config_data()
                .await
                .expect_err("gzip content-encoding must fail closed"),
            LsError::UnsupportedCompression {
                rpc: "GetCascadeModelConfigData"
            }
        );
        gzip_handle.abort();

        // Connect-Content-Encoding: gzip -> fail closed UnsupportedCompression
        let (connect_gzip_port, connect_gzip_handle) = spawn_ipv4_router(Router::new().route(
            RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES,
            post(|| async {
                (
                    StatusCode::OK,
                    [
                        (
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/connect+json"),
                        ),
                        (
                            HeaderName::from_static(HEADER_CONNECT_CONTENT_ENCODING),
                            HeaderValue::from_static("gzip"),
                        ),
                    ],
                    "",
                )
            }),
        ))
        .await;
        let connect_gzip_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{connect_gzip_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            connect_gzip_client
                .subscribe_to_summaries()
                .await
                .expect_err("gzip connect-content-encoding must fail closed"),
            LsError::UnsupportedCompression {
                rpc: "JetboxSubscribeToSummaries"
            }
        );
        connect_gzip_handle.abort();
    }

    #[tokio::test]
    async fn enforces_unary_512_kib_and_4_mib_response_size_caps() {
        // (a) GetCascadeModelConfigData > 512 KiB rejected via streaming chunked body
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = [0u8; 2048];
                let _ = sock.read(&mut req_buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                    )
                    .await;
                let chunk = vec![b'a'; 64 * 1024];
                let hdr = format!("{:x}\r\n", chunk.len());
                for _ in 0..9 {
                    if sock.write_all(hdr.as_bytes()).await.is_err()
                        || sock.write_all(&chunk).await.is_err()
                        || sock.write_all(b"\r\n").await.is_err()
                    {
                        break;
                    }
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
        });

        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            client
                .get_cascade_model_config_data()
                .await
                .expect_err("512 KiB cap must trigger"),
            LsError::ResponseTooLarge {
                rpc: "GetCascadeModelConfigData",
                max_bytes: MAX_LS_MODEL_CONFIG_BYTES,
            }
        );
        handle.abort();

        // (b) GetCascadeTrajectorySteps > 4 MiB rejected upfront via Content-Length
        let cl_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let cl_port = cl_listener.local_addr().unwrap().port();
        let cl_handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = cl_listener.accept().await {
                let mut req_buf = [0u8; 2048];
                let _ = sock.read(&mut req_buf).await;
                let oversized = MAX_LS_RESPONSE_BYTES + 1;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {oversized}\r\n\r\n"
                );
                let _ = sock.write_all(headers.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        let cl_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{cl_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cl_client
                .get_cascade_trajectory_steps("conv-123456", 0)
                .await
                .expect_err("4 MiB cap must trigger"),
            LsError::ResponseTooLarge {
                rpc: "GetCascadeTrajectorySteps",
                max_bytes: MAX_LS_RESPONSE_BYTES,
            }
        );
        cl_handle.abort();
    }

    #[tokio::test]
    async fn allowlist_dtos_parse_string_and_numeric_int64_ignore_prompts_and_parse_3way_fingerprints()
     {
        let captured_steps_req = Arc::new(Mutex::new(String::new()));
        let captured_steps_req_clone = Arc::clone(&captured_steps_req);

        let (port, handle) = spawn_ipv4_router(
            Router::new()
                .route(
                    RPC_GET_CASCADE_TRAJECTORY_STEPS,
                    post(move |body: Bytes| {
                        let cap = Arc::clone(&captured_steps_req_clone);
                        async move {
                            *cap.lock().await = String::from_utf8_lossy(&body).to_string();
                            (
                                StatusCode::OK,
                                [(header::CONTENT_TYPE, "application/json")],
                                r#"{
                                    "steps": [
                                        {
                                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                                            "status": "CORTEX_STEP_STATUS_DONE",
                                            "plannerResponse": {
                                                "response": "SECRET_ASSISTANT_TEXT_MUST_BE_IGNORED",
                                                "thinking": "SECRET_THINKING_TEXT"
                                            },
                                            "userInput": {
                                                "items": [{"text": "SECRET_USER_PROMPT"}]
                                            },
                                            "metadata": {
                                                "createdAt": "2026-11-26T18:57:30.100Z",
                                                "viewableAt": "2026-11-26T18:57:30.510Z",
                                                "finishedGeneratingAt": "2026-11-26T18:57:33.098Z",
                                                "completedAt": "2026-11-26T18:57:33.098Z",
                                                "startedAt": "2026-11-26T18:57:33.098Z",
                                                "generatorModel": "MODEL_PLACEHOLDER_M335",
                                                "modelUsage": {
                                                    "model": "MODEL_PLACEHOLDER_M335",
                                                    "inputTokens": "2177",
                                                    "outputTokens": "335",
                                                    "cacheReadTokens": "32952",
                                                    "cacheWriteTokens": 0,
                                                    "responseOutputTokens": "283",
                                                    "thinkingOutputTokens": 52,
                                                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                                    "responseId": "G4EIa9nlDOGfur8P9r-ngQo",
                                                    "responseHeader": {
                                                        "responseId": "G4EIa9nlDOGfur8P9r-ngQo",
                                                        "x-secret-cookie": "must-not-be-captured"
                                                    }
                                                },
                                                "retryInfos": [
                                                    {
                                                        "sherlogLink": "https://internal/secret",
                                                        "error": "transient error text ignored",
                                                        "retryReason": "RETRY_REASON_API_RETRYABLE",
                                                        "usage": { "model": "MODEL_PLACEHOLDER_M335" }
                                                    },
                                                    {
                                                        "usage": {
                                                            "model": "MODEL_PLACEHOLDER_M335",
                                                            "inputTokens": 2177,
                                                            "outputTokens": 335,
                                                            "cacheReadTokens": 32952,
                                                            "responseOutputTokens": 283,
                                                            "thinkingOutputTokens": 52,
                                                            "responseId": "G4EIa9nlDOGfur8P9r-ngQo"
                                                        }
                                                    }
                                                ]
                                            }
                                        },
                                        {
                                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                                            "status": "CORTEX_STEP_STATUS_DONE",
                                            "metadata": {
                                                "generatorModel": "MODEL_PLACEHOLDER_M467",
                                                "modelUsage": {
                                                    "model": "MODEL_PLACEHOLDER_M467",
                                                    "inputTokens": 1000,
                                                    "outputTokens": 200,
                                                    "apiProvider": "API_PROVIDER_GOOGLE_VERTEX",
                                                    "responseId": "AJ8Ia5zbOOCR9LsPw_yNoQs",
                                                    "responseHeader": { "sessionID": "builtin-session-1" }
                                                }
                                            }
                                        },
                                        {
                                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                                            "status": "CORTEX_STEP_STATUS_DONE",
                                            "metadata": {
                                                "generatorModel": "MODEL_GOOGLE_GEMINI_INTERNAL_BYOM",
                                                "modelInfo": {
                                                    "modelId": "MODEL_GOOGLE_GEMINI_INTERNAL_BYOM",
                                                    "modelName": "gemini-4-hydrogen",
                                                    "modelUrl": "gemini-api://gemini-4-hydrogen",
                                                    "displayName": "Custom Direct"
                                                },
                                                "modelUsage": {
                                                    "inputTokens": "500",
                                                    "outputTokens": "100",
                                                    "responseHeader": {
                                                        "responseId": "direct-resp-1",
                                                        "sessionId": "direct-session-lower"
                                                    }
                                                }
                                            }
                                        }
                                    ]
                                }"#,
                            )
                        }
                    }),
                )
                .route(
                    RPC_GET_ALL_CASCADE_TRAJECTORIES,
                    post(|| async {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{
                                "trajectorySummaries": {
                                    "sub-123456": {
                                        "summary": "SECRET_USER_PROMPT_SUMMARY_MUST_BE_IGNORED",
                                        "workspaces": [{"workspaceFolderAbsoluteUri": "file:///secret"}],
                                        "trajectoryId": "traj-abc",
                                        "trajectoryType": "CORTEX_TRAJECTORY_TYPE_CASCADE",
                                        "stepCount": 42,
                                        "status": "CASCADE_RUN_STATUS_IDLE",
                                        "source": "CORTEX_TRAJECTORY_SOURCE_CASCADE_CLIENT",
                                        "createdTime": "2026-11-26T18:00:00Z",
                                        "lastModifiedTime": "2026-11-26T18:59:00Z",
                                        "trajectoryMetadata": {
                                            "parentConversationId": "root-123456",
                                            "rootConversationId": "root-123456",
                                            "nestingDepth": 1
                                        }
                                    }
                                }
                            }"#,
                        )
                    }),
                ),
        )
        .await;

        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();

        let steps = client
            .get_cascade_trajectory_steps("conv-a1262862", 10)
            .await
            .expect("steps RPC");
        assert_eq!(steps.len(), 3);

        // Verify request payload enforces PROD_UI (2) and disableRehydration: true
        let sent_req = captured_steps_req.lock().await.clone();
        assert!(sent_req.contains(r#""cascadeId":"conv-a1262862""#));
        assert!(sent_req.contains(r#""stepOffset":10"#));
        assert!(sent_req.contains(r#""trajectoryVerbosity":2"#));
        assert!(sent_req.contains(r#""disableRehydration":true"#));

        // Verify step 0: string + numeric int64 parsing, proxy fingerprint, retryInfos
        let s0_meta = steps[0].metadata.as_ref().unwrap();
        let s0_usage = s0_meta.model_usage.as_ref().unwrap();
        assert_eq!(
            steps[0].step_type.as_deref(),
            Some("CORTEX_STEP_TYPE_PLANNER_RESPONSE")
        );
        assert_eq!(s0_usage.input_tokens, 2177);
        assert_eq!(s0_usage.output_tokens, 335);
        assert_eq!(s0_usage.cache_read_tokens, 32952);
        assert_eq!(s0_usage.cache_write_tokens, 0);
        assert_eq!(s0_usage.response_output_tokens, 283);
        assert_eq!(s0_usage.thinking_output_tokens, 52);
        assert_eq!(
            s0_usage.response_id.as_deref(),
            Some("G4EIa9nlDOGfur8P9r-ngQo")
        );
        let fp0 = s0_usage.response_header.as_ref().unwrap();
        assert!(fp0.has_response_id());
        assert!(!fp0.has_builtin_session_id());
        assert!(!fp0.has_direct_session_id());
        assert_eq!(s0_meta.retry_infos.len(), 2);
        assert_eq!(
            s0_meta.retry_infos[0].usage.as_ref().unwrap().input_tokens,
            0
        );
        assert_eq!(
            s0_meta.retry_infos[1].usage.as_ref().unwrap().input_tokens,
            2177
        );

        // Verify step 1: built-in CodeAssistClient fingerprint (`sessionID` upper)
        let fp1 = steps[1]
            .metadata
            .as_ref()
            .unwrap()
            .model_usage
            .as_ref()
            .unwrap()
            .response_header
            .as_ref()
            .unwrap();
        assert!(fp1.has_builtin_session_id());
        assert!(!fp1.has_direct_session_id());

        // Verify step 2: direct AI Studio fingerprint (`responseId` + `sessionId` lower) + modelInfo
        let s2_meta = steps[2].metadata.as_ref().unwrap();
        let fp2 = s2_meta
            .model_usage
            .as_ref()
            .unwrap()
            .response_header
            .as_ref()
            .unwrap();
        assert!(fp2.has_response_id());
        assert!(fp2.has_direct_session_id());
        assert!(!fp2.has_builtin_session_id());
        assert_eq!(
            s2_meta.model_info.as_ref().unwrap().model_url.as_deref(),
            Some("gemini-api://gemini-4-hydrogen")
        );

        // Ensure prompt/secret strings were never retained anywhere in Debug output
        let steps_debug = format!("{steps:?}");
        assert!(!steps_debug.contains("SECRET_ASSISTANT_TEXT"));
        assert!(!steps_debug.contains("SECRET_USER_PROMPT"));
        assert!(!steps_debug.contains("must-not-be-captured"));
        assert!(!steps_debug.contains("https://internal/secret"));

        // Verify GetAllCascadeTrajectories allowlist DTO
        let summaries = client
            .get_all_cascade_trajectories(false)
            .await
            .expect("get_all_cascade_trajectories");
        assert_eq!(summaries.len(), 1);
        let sub = summaries.get("sub-123456").expect("subagent summary");
        assert_eq!(sub.trajectory_id.as_deref(), Some("traj-abc"));
        assert_eq!(sub.step_count, 42);
        let meta = sub.trajectory_metadata.as_ref().unwrap();
        assert_eq!(meta.parent_conversation_id.as_deref(), Some("root-123456"));
        assert_eq!(meta.root_conversation_id.as_deref(), Some("root-123456"));
        assert_eq!(meta.nesting_depth, 1);
        assert!(!format!("{summaries:?}").contains("SECRET_USER_PROMPT_SUMMARY"));
        handle.abort();
    }

    #[tokio::test]
    async fn connect_stream_parses_split_chunks_clean_endstream_and_error_codes_fail_closed() {
        // (a) Multi-frame response split across odd/1-byte TCP chunks + clean 0x02 EndStream
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received_req_bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        let received_req_bytes_clone = Arc::clone(&received_req_bytes);

        let handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = vec![0u8; 4096];
                let mut total = 0usize;
                loop {
                    let n = sock.read(&mut req_buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if let Some(pos) = req_buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
                        let body_start = pos + 4;
                        if total >= body_start + CONNECT_STREAM_EMPTY_JSON_ENVELOPE.len() {
                            break;
                        }
                    }
                }
                *received_req_bytes_clone.lock().await = req_buf[..total].to_vec();

                let mut stream_bytes = Vec::new();
                stream_bytes.extend_from_slice(&encode_connect_frame(
                    0x00,
                    br#"{"updates":{"conv-1":{"trajectoryId":"t1","stepCount":5,"summary":"IGNORED"}}}"#,
                ));
                stream_bytes.extend_from_slice(&encode_connect_frame(
                    0x00,
                    br#"{"updates":{"conv-2":{"trajectoryId":"t2","stepCount":12}},"deletes":["conv-old"]}"#,
                ));
                stream_bytes
                    .extend_from_slice(&encode_connect_frame(CONNECT_FLAG_END_STREAM, b"{}"));

                let resp_head = "HTTP/1.1 200 OK\r\nContent-Type: application/connect+json\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = sock.write_all(resp_head.as_bytes()).await;

                // Drip in awkward 1-byte and 3-byte chunks across envelope boundaries!
                let mut idx = 0;
                let mut step = 1;
                while idx < stream_bytes.len() {
                    let end = (idx + step).min(stream_bytes.len());
                    let slice = &stream_bytes[idx..end];
                    let chunk = format!("{:x}\r\n", slice.len());
                    let _ = sock.write_all(chunk.as_bytes()).await;
                    let _ = sock.write_all(slice).await;
                    let _ = sock.write_all(b"\r\n").await;
                    let _ = sock.flush().await;
                    idx = end;
                    step = if step == 1 {
                        3
                    } else if step == 3 {
                        7
                    } else {
                        1
                    };
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
        });

        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        let mut stream = client
            .subscribe_to_summaries()
            .await
            .expect("open Connect summary stream");

        let d1 = stream
            .next_delta()
            .await
            .expect("frame 1 ok")
            .expect("frame 1 some");
        assert_eq!(d1.updates.len(), 1);
        assert_eq!(d1.updates["conv-1"].step_count, 5);
        assert_eq!(d1.updates["conv-1"].trajectory_id.as_deref(), Some("t1"));

        let d2 = stream
            .next_delta()
            .await
            .expect("frame 2 ok")
            .expect("frame 2 some");
        assert_eq!(d2.updates["conv-2"].step_count, 12);
        assert_eq!(d2.deletes, vec!["conv-old".to_string()]);

        // Clean EndStream 0x02 {} returns Ok(None)
        assert_eq!(stream.next_delta().await.expect("clean endstream"), None);
        assert_eq!(stream.next_delta().await.expect("subsequent eof"), None);

        // Verify exact Connect streaming request headers + 7-byte envelope body
        let raw_req = received_req_bytes.lock().await;
        let head_end = raw_req
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("http headers");
        let head_str = String::from_utf8_lossy(&raw_req[..head_end]).to_ascii_lowercase();
        assert!(head_str.contains("content-type: application/connect+json"));
        assert!(head_str.contains("connect-protocol-version: 1"));
        assert!(head_str.contains("connect-accept-encoding: identity"));
        assert!(head_str.contains("accept-encoding: identity"));
        assert!(head_str.contains(&format!(
            "{HEADER_ANTIGRAVITY_CSRF_TOKEN}: {}",
            TEST_CSRF_SECRET.to_ascii_lowercase()
        )));
        assert!(head_str.contains(&format!(
            "{HEADER_CODEIUM_CSRF_TOKEN}: {}",
            TEST_CSRF_SECRET.to_ascii_lowercase()
        )));
        assert_eq!(
            &raw_req[head_end + 4..head_end + 4 + 7],
            &CONNECT_STREAM_EMPTY_JSON_ENVELOPE
        );
        handle.abort();

        // (b) Compressed Connect frame (flags & 0x01 != 0) fails closed immediately
        let (comp_port, comp_handle) = spawn_ipv4_router(Router::new().route(
            RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES,
            post(|| async {
                let frame = encode_connect_frame(CONNECT_FLAG_COMPRESSED, b"compressed-bytes");
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/connect+json")],
                    frame,
                )
            }),
        ))
        .await;
        let comp_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{comp_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        let mut comp_stream = comp_client.subscribe_to_summaries().await.unwrap();
        assert_eq!(
            comp_stream
                .next_delta()
                .await
                .expect_err("compressed frame must fail closed"),
            LsError::UnsupportedCompression {
                rpc: "JetboxSubscribeToSummaries"
            }
        );
        comp_handle.abort();

        // (c) Oversized Connect frame (> 4 MiB header length) fails closed BEFORE waiting for body
        let (big_port, big_handle) = spawn_ipv4_router(Router::new().route(
            RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES,
            post(|| async {
                let mut bad_hdr = Vec::with_capacity(5);
                bad_hdr.push(0x00);
                bad_hdr.extend_from_slice(&((MAX_LS_RESPONSE_BYTES as u32) + 1).to_be_bytes());
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/connect+json")],
                    bad_hdr,
                )
            }),
        ))
        .await;
        let big_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{big_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        let mut big_stream = big_client.subscribe_to_summaries().await.unwrap();
        assert_eq!(
            big_stream
                .next_delta()
                .await
                .expect_err("oversized frame header must fail closed"),
            LsError::ResponseTooLarge {
                rpc: "JetboxSubscribeToSummaries",
                max_bytes: MAX_LS_RESPONSE_BYTES,
            }
        );
        big_handle.abort();

        // (d) EndStream 0x02 error mapping (`unimplemented`, `permission_denied`, `unauthenticated`, other)
        for (code_str, expected_err) in [
            (
                "unimplemented",
                LsError::Unimplemented {
                    rpc: "JetboxSubscribeToSummaries",
                },
            ),
            (
                "permission_denied",
                LsError::PermissionDenied {
                    rpc: "JetboxSubscribeToSummaries",
                },
            ),
            (
                "unauthenticated",
                LsError::Unauthenticated {
                    rpc: "JetboxSubscribeToSummaries",
                },
            ),
            (
                "failed_precondition",
                LsError::ConnectError {
                    rpc: "JetboxSubscribeToSummaries",
                    code: "failed_precondition".to_string(),
                },
            ),
        ] {
            let payload = format!(
                r#"{{"error":{{"code":"{code_str}","message":"jetbox summaries store not initialized"}}}}"#
            );
            let frame = encode_connect_frame(CONNECT_FLAG_END_STREAM, payload.as_bytes());
            let (e_port, e_handle) = spawn_ipv4_router(Router::new().route(
                RPC_JETBOX_SUBSCRIBE_TO_SUMMARIES,
                post(move || {
                    let f = frame.clone();
                    async move {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/connect+json")],
                            f,
                        )
                    }
                }),
            ))
            .await;
            let e_client = LsClient::new(
                LsEndpoint::parse(&format!("127.0.0.1:{e_port}")).unwrap(),
                CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
            )
            .unwrap();
            let mut e_stream = e_client.subscribe_to_summaries().await.unwrap();
            let err = e_stream
                .next_delta()
                .await
                .expect_err("endstream error must fail");
            assert_eq!(err, expected_err);
            assert!(!err.to_string().contains("store not initialized"));
            e_handle.abort();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_cascade_trajectory_generator_metadata_parses_str_and_num_ignores_unknown_and_sends_include_messages_false()
     {
        let captured = Arc::new(tokio::sync::Mutex::new(
            Option::<(HeaderMap, Vec<u8>)>::None,
        ));
        let captured_clone = Arc::clone(&captured);

        let app = Router::new().route(
            RPC_GET_CASCADE_TRAJECTORY_GENERATOR_METADATA,
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = Arc::clone(&captured_clone);
                async move {
                    *captured.lock().await = Some((headers, body.to_vec()));
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{
                            "generatorMetadata": [
                                {
                                    "unknownTopField": {"nested": [1, 2, 3]},
                                    "chatModel": {
                                        "extraChatModelField": "ignored",
                                        "systemPrompt": "ignored large prompt",
                                        "tools": [{"name": "ignored"}],
                                        "model": "MODEL_PLACEHOLDER_M467",
                                        "responseModel": "gemini-4-neon",
                                        "modelDisplayName": "Gemini 4 Neon",
                                        "usage": {
                                            "model": "MODEL_PLACEHOLDER_M467",
                                            "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                            "inputTokens": "1024",
                                            "outputTokens": "256",
                                            "responseHeader": {
                                                "sessionID": "agyc_proxy_sess"
                                            }
                                        },
                                        "chatStartMetadata": {
                                            "contextWindowMetadata": {
                                                "maxContextTokens": "512000",
                                                "estimatedTokensUsed": "77000"
                                            }
                                        }
                                    }
                                },
                                {
                                    "chatModel": {
                                        "model": 467,
                                        "chatStartMetadata": {
                                            "contextWindowMetadata": {
                                                "maxContextTokens": 200000
                                            }
                                        }
                                    }
                                },
                                {
                                    "chatModel": {
                                        "chatStartMetadata": {
                                            "contextWindowMetadata": {
                                                "maxContextTokens": 0
                                            }
                                        }
                                    }
                                },
                                {
                                    "chatModel": {}
                                }
                            ]
                        }"#,
                    )
                }
            }),
        );

        let (port, handle) = spawn_ipv4_router(app).await;
        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();

        let entries = client
            .get_cascade_trajectory_generator_metadata("cascade-abc", 3)
            .await
            .expect("generator metadata should parse");
        handle.abort();

        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].max_context_tokens(), Some(512_000));
        let chat0 = entries[0].chat_model.as_ref().expect("chat_model 0");
        assert_eq!(chat0.model.as_deref(), Some("MODEL_PLACEHOLDER_M467"));
        assert_eq!(chat0.response_model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(chat0.model_display_name.as_deref(), Some("Gemini 4 Neon"));
        let usage0 = chat0.usage.as_ref().expect("usage 0");
        assert_eq!(usage0.model.as_deref(), Some("MODEL_PLACEHOLDER_M467"));
        assert_eq!(
            usage0.api_provider.as_deref(),
            Some("API_PROVIDER_GOOGLE_GEMINI")
        );
        assert_eq!(usage0.input_tokens, 1024);
        assert_eq!(usage0.output_tokens, 256);
        assert_eq!(
            usage0
                .response_header
                .as_ref()
                .and_then(|h| h.session_id_upper.as_deref()),
            Some("agyc_proxy_sess")
        );
        assert_eq!(entries[1].max_context_tokens(), Some(200_000));
        assert_eq!(
            entries[1]
                .chat_model
                .as_ref()
                .and_then(|c| c.model.as_deref()),
            Some("467")
        );
        assert_eq!(entries[2].max_context_tokens(), None);
        assert_eq!(entries[3].max_context_tokens(), None);

        let (req_headers, req_body) = captured
            .lock()
            .await
            .clone()
            .expect("request should have been captured");
        assert_eq!(
            req_headers
                .get(HEADER_ANTIGRAVITY_CSRF_TOKEN)
                .and_then(|v| v.to_str().ok()),
            Some(TEST_CSRF_SECRET)
        );
        assert_eq!(
            req_headers
                .get(HEADER_CODEIUM_CSRF_TOKEN)
                .and_then(|v| v.to_str().ok()),
            Some(TEST_CSRF_SECRET)
        );
        let req_json: serde_json::Value =
            serde_json::from_slice(&req_body).expect("request body must be valid JSON");
        assert_eq!(
            req_json,
            serde_json::json!({
                "cascadeId": "cascade-abc",
                "generatorMetadataOffset": 3,
                "includeMessages": false,
            })
        );

        // 404 maps to LsError::Unimplemented
        let (unimpl_port, unimpl_handle) = spawn_ipv4_router(Router::new().route(
            RPC_GET_CASCADE_TRAJECTORY_GENERATOR_METADATA,
            post(|| async { (StatusCode::NOT_FOUND, r#"{"code":"unimplemented"}"#) }),
        ))
        .await;
        let unimpl_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{unimpl_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            unimpl_client
                .get_cascade_trajectory_generator_metadata("cascade-abc", 0)
                .await
                .expect_err("404 must map to Unimplemented"),
            LsError::Unimplemented {
                rpc: "GetCascadeTrajectoryGeneratorMetadata"
            }
        );
        unimpl_handle.abort();

        // Oversized response (> MAX_LS_RESPONSE_BYTES) maps to ResponseTooLarge
        let (big_port, big_handle) = spawn_ipv4_router(Router::new().route(
            RPC_GET_CASCADE_TRAJECTORY_GENERATOR_METADATA,
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    vec![b' '; MAX_LS_RESPONSE_BYTES + 1],
                )
            }),
        ))
        .await;
        let big_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{big_port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();
        assert_eq!(
            big_client
                .get_cascade_trajectory_generator_metadata("cascade-abc", 0)
                .await
                .expect_err("oversized response must fail closed"),
            LsError::ResponseTooLarge {
                rpc: "GetCascadeTrajectoryGeneratorMetadata",
                max_bytes: MAX_LS_RESPONSE_BYTES,
            }
        );
        big_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_cascade_model_config_data_merges_cached_and_battle_mode_with_client_precedence() {
        let app = Router::new().route(
            RPC_GET_CASCADE_MODEL_CONFIG_DATA,
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{
                        "clientModelConfigs": [
                            {
                                "label": "Client Hydrogen (High)",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M525" },
                                "modelId": "gemini-4-hydrogen-high"
                            }
                        ],
                        "cachedCascadeModelConfigs": [
                            {
                                "label": "Stale Cached Hydrogen (High)",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M525" },
                                "modelId": "stale-cached-id"
                            },
                            {
                                "label": "Cached Hydrogen Tiered (High)",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M530" },
                                "modelId": "gemini-4-hydrogen-tiered-high"
                            }
                        ],
                        "battleModeModelConfigs": [
                            {
                                "label": "Battle Mode Neon",
                                "modelOrAlias": { "model": 532 },
                                "modelId": "gemini-4-neon-high"
                            }
                        ]
                    }"#,
                )
            }),
        );

        let (port, handle) = spawn_ipv4_router(app).await;
        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();

        let configs = client
            .get_cascade_model_config_data()
            .await
            .expect("merged configs");
        handle.abort();

        let by_model: HashMap<String, LsModelConfig> = configs
            .into_iter()
            .filter_map(|cfg| {
                let m = cfg.model_or_alias.as_ref()?.model.clone()?;
                Some((m, cfg))
            })
            .collect();
        assert_eq!(
            by_model
                .get("MODEL_PLACEHOLDER_M525")
                .and_then(|c| c.model_id.as_deref()),
            Some("gemini-4-hydrogen-high"),
            "clientModelConfigs must take precedence over cachedCascadeModelConfigs"
        );
        assert_eq!(
            by_model
                .get("MODEL_PLACEHOLDER_M530")
                .and_then(|c| c.model_id.as_deref()),
            Some("gemini-4-hydrogen-tiered-high")
        );
        assert_eq!(
            by_model.get("532").and_then(|c| c.model_id.as_deref()),
            Some("gemini-4-neon-high")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_cascade_model_config_data_merges_missing_fields_under_partial_client_entry() {
        let app = Router::new().route(
            RPC_GET_CASCADE_MODEL_CONFIG_DATA,
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{
                        "clientModelConfigs": [
                            {
                                "label": "",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M991" },
                                "provider": null,
                                "modelUrl": "",
                                "modelId": null
                            },
                            {
                                "label": "Client Override Label",
                                "modelOrAlias": { "model": 992 },
                                "provider": "MODEL_PROVIDER_UNSPECIFIED",
                                "modelId": "MODEL_PLACEHOLDER_M992"
                            },
                            {
                                "label": "Client M993",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M993" },
                                "provider": "MODEL_PROVIDER_GOOGLE",
                                "modelId": "gemini-5-epsilon-high"
                            }
                        ],
                        "cachedCascadeModelConfigs": [
                            {
                                "label": "Gemini 5 Gamma (High)",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M991" },
                                "provider": "API_PROVIDER_GOOGLE_GEMINI",
                                "modelUrl": "gemini-api://gemini-5-gamma",
                                "modelId": "gemini-5-gamma-high",
                                "contextWindowOptions": [{"name": "1M", "expensive": false}]
                            },
                            {
                                "label": "Stale Cached Label",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M992" },
                                "provider": "MODEL_PROVIDER_GOOGLE",
                                "modelId": "gemini-5-delta-high"
                            },
                            {
                                "label": "Cached M993",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M993" },
                                "provider": "MODEL_PROVIDER_OPENAI",
                                "modelId": "stale-epsilon-id"
                            }
                        ],
                        "battleModeModelConfigs": [
                            {
                                "label": "Battle Label",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M992", "alias": "ALIAS_M992" },
                                "modelUrl": "gemini-api://gemini-5-delta",
                                "modelId": "battle-delta-id",
                                "contextWindowOptions": [{"name": "2M", "expensive": true}]
                            },
                            {
                                "label": "Alias Fallback Should Not Override",
                                "modelOrAlias": { "alias": "ALIAS_M992" },
                                "modelId": "alias-should-not-override"
                            }
                        ]
                    }"#,
                )
            }),
        );

        let (port, handle) = spawn_ipv4_router(app).await;
        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).unwrap(),
            CsrfToken::parse(TEST_CSRF_SECRET).unwrap(),
        )
        .unwrap();

        let configs = client
            .get_cascade_model_config_data()
            .await
            .expect("merged configs");
        handle.abort();

        assert_eq!(
            configs.len(),
            3,
            "duplicate/partial keys across sections must merge into 3 entries"
        );

        let by_key: HashMap<String, LsModelConfig> = configs
            .into_iter()
            .filter_map(|cfg| {
                let raw = cfg.model_or_alias.as_ref()?.model.as_deref()?;
                let norm = companion_core::normalize_placeholder_enum_key(raw)
                    .unwrap_or_else(|| raw.to_string());
                Some((norm, cfg))
            })
            .collect();

        let m991 = by_key.get("MODEL_PLACEHOLDER_M991").expect("M991 present");
        assert_eq!(m991.model_id.as_deref(), Some("gemini-5-gamma-high"));
        assert_eq!(m991.label.as_deref(), Some("Gemini 5 Gamma (High)"));
        assert_eq!(m991.provider.as_deref(), Some("API_PROVIDER_GOOGLE_GEMINI"));
        assert_eq!(
            m991.model_url.as_deref(),
            Some("gemini-api://gemini-5-gamma")
        );
        assert_eq!(m991.context_window_options.len(), 1);
        assert_eq!(m991.context_window_options[0].name.as_deref(), Some("1M"));

        let m992 = by_key.get("MODEL_PLACEHOLDER_M992").expect("M992 present");
        assert_eq!(
            m992.label.as_deref(),
            Some("Client Override Label"),
            "populated client label must take precedence"
        );
        assert_eq!(
            m992.model_id.as_deref(),
            Some("gemini-5-delta-high"),
            "placeholder modelId in client entry must yield to resolved cached modelId"
        );
        assert_eq!(m992.provider.as_deref(), Some("MODEL_PROVIDER_GOOGLE"));
        assert_eq!(
            m992.model_url.as_deref(),
            Some("gemini-api://gemini-5-delta"),
            "missing modelUrl must backfill from battleModeModelConfigs"
        );
        assert_eq!(m992.context_window_options.len(), 1);
        assert_eq!(m992.context_window_options[0].name.as_deref(), Some("2M"));
        assert_eq!(
            m992.model_or_alias
                .as_ref()
                .and_then(|m| m.alias.as_deref()),
            Some("ALIAS_M992")
        );

        let m993 = by_key.get("MODEL_PLACEHOLDER_M993").expect("M993 present");
        assert_eq!(m993.label.as_deref(), Some("Client M993"));
        assert_eq!(m993.model_id.as_deref(), Some("gemini-5-epsilon-high"));
        assert_eq!(m993.provider.as_deref(), Some("MODEL_PROVIDER_GOOGLE"));
    }

    #[test]
    fn generator_metadata_deserializes_chat_model_fields_and_step_indices() {
        let json_str = r#"{
            "generatorMetadata": [
                {
                    "stepIndices": [26, "27"],
                    "chatModel": {
                        "model": "MODEL_PLACEHOLDER_M530",
                        "responseModel": "gemini-4-hydrogen-tiered-high",
                        "modelDisplayName": "Gemini 4 Hydrogen (High)",
                        "chatStartMetadata": {
                            "contextWindowMetadata": { "maxContextTokens": 512000 }
                        }
                    }
                },
                {
                    "stepIndices": [30],
                    "chatModel": {
                        "model": 531,
                        "responseModel": "gemini-4-hydrogen-tiered-medium",
                        "modelDisplayName": "Gemini 4 Hydrogen (Medium)"
                    }
                }
            ]
        }"#;
        let parsed: GetCascadeTrajectoryGeneratorMetadataResponse =
            serde_json::from_str(json_str).expect("parse generator metadata");
        assert_eq!(parsed.generator_metadata.len(), 2);
        let g0 = &parsed.generator_metadata[0];
        assert_eq!(g0.step_indices, vec![26, 27]);
        let cm0 = g0.chat_model.as_ref().expect("chat_model 0");
        assert_eq!(cm0.model.as_deref(), Some("MODEL_PLACEHOLDER_M530"));
        assert_eq!(
            cm0.response_model.as_deref(),
            Some("gemini-4-hydrogen-tiered-high")
        );
        assert_eq!(
            cm0.model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen (High)")
        );
        assert_eq!(g0.max_context_tokens(), Some(512_000));

        let g1 = &parsed.generator_metadata[1];
        assert_eq!(g1.step_indices, vec![30]);
        let cm1 = g1.chat_model.as_ref().expect("chat_model 1");
        assert_eq!(cm1.model.as_deref(), Some("531"));
        assert_eq!(
            cm1.response_model.as_deref(),
            Some("gemini-4-hydrogen-tiered-medium")
        );
    }
}
