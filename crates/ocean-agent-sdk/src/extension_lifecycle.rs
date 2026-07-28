//! Strict, metadata-only DTOs for the `ocean.extension.service` protocol v1.
//!
//! This module defines wire vocabulary only. It owns no process, transport,
//! registry, replay, or execution behavior.

use std::{error::Error, fmt};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{
    de::DeserializeOwned, ser::SerializeStruct, Deserialize, Deserializer, Serialize, Serializer,
};
use uuid::Uuid;

/// Protocol name carried by every service frame.
pub const PROTOCOL_NAME: &str = "ocean.extension.service";
/// Only protocol version accepted by this DTO module.
pub const PROTOCOL_VERSION: u8 = 1;
/// Maximum encoded NDJSON frame size, including its trailing newline.
pub const MAX_FRAME_BYTES: usize = 65_536;
/// Maximum UTF-8 byte length of a lifecycle tool identifier.
pub const MAX_TOOL_NAME_BYTES: usize = 256;

/// Error returned by strict NDJSON encoding or decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The encoded frame exceeds [`MAX_FRAME_BYTES`].
    TooLarge { encoded_bytes: usize },
    /// The frame is not exactly one JSON object followed by one newline.
    InvalidFraming,
    /// JSON or DTO validation failed.
    InvalidFrame(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { encoded_bytes } => {
                write!(
                    f,
                    "encoded frame is {encoded_bytes} bytes; maximum is {MAX_FRAME_BYTES}"
                )
            }
            Self::InvalidFraming => {
                f.write_str("frame must be one JSON object followed by newline")
            }
            Self::InvalidFrame(message) => write!(f, "invalid frame: {message}"),
        }
    }
}

impl Error for FrameError {}

/// Encode one strict NDJSON frame, including its trailing newline.
pub fn encode_frame<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameError> {
    let mut encoded =
        serde_json::to_vec(frame).map_err(|error| FrameError::InvalidFrame(error.to_string()))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            encoded_bytes: encoded.len(),
        });
    }
    Ok(encoded)
}

/// Decode exactly one strict NDJSON frame.
///
/// Blank input, missing or duplicate newlines, leading/trailing whitespace,
/// arrays/scalars, invalid UTF-8, trailing bytes, and oversized input fail.
pub fn decode_frame<T: DeserializeOwned>(encoded: &[u8]) -> Result<T, FrameError> {
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            encoded_bytes: encoded.len(),
        });
    }
    if encoded.len() < 3
        || encoded.first() != Some(&b'{')
        || encoded.last() != Some(&b'\n')
        || encoded.get(encoded.len() - 2) != Some(&b'}')
        || encoded[..encoded.len() - 1].contains(&b'\n')
        || encoded[..encoded.len() - 1].contains(&b'\r')
    {
        return Err(FrameError::InvalidFraming);
    }
    serde_json::from_slice(&encoded[..encoded.len() - 1])
        .map_err(|error| FrameError::InvalidFrame(error.to_string()))
}

/// Exact protocol-name marker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtocolName;

impl Serialize for ProtocolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(PROTOCOL_NAME)
    }
}

impl<'de> Deserialize<'de> for ProtocolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == PROTOCOL_NAME {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("unsupported protocol"))
        }
    }
}

/// Exact protocol-version marker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtocolV1;

impl Serialize for ProtocolV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ProtocolV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        if value == u64::from(PROTOCOL_VERSION) {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("unsupported protocol version"))
        }
    }
}

macro_rules! string_marker {
    ($name:ident, $wire:literal) => {
        #[doc = concat!("Exact `", $wire, "` frame marker.")]
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str($wire)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                if value == $wire {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom(concat!("expected ", $wire)))
                }
            }
        }
    };
}

string_marker!(HostHelloFrame, "host_hello");
string_marker!(ServiceHelloFrame, "service_hello");
string_marker!(ReadyFrame, "ready");
string_marker!(EventFrame, "event");
string_marker!(AckFrame, "ack");
string_marker!(PongFrame, "pong");
string_marker!(StatusFrame, "status");
string_marker!(ShutdownCompleteFrame, "shutdown_complete");
string_marker!(LagFrame, "lag");
string_marker!(ResetFrame, "reset");
string_marker!(PingFrame, "ping");
string_marker!(ShutdownFrame, "shutdown");

/// A decimal `u64` encoded as a JSON string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(pub u64);

impl Serialize for Sequence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
            return Err(serde::de::Error::custom(
                "sequence must be canonical decimal",
            ));
        }
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|_| serde::de::Error::custom("sequence must be a decimal u64"))
    }
}

/// UTC timestamp encoded in canonical RFC3339 millisecond form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MillisecondTimestamp(DateTime<Utc>);

impl MillisecondTimestamp {
    /// Construct a timestamp, truncating sub-millisecond precision.
    #[must_use]
    pub fn new(value: DateTime<Utc>) -> Self {
        let nanos = value.timestamp_subsec_nanos();
        let truncated = value - chrono::Duration::nanoseconds(i64::from(nanos % 1_000_000));
        Self(truncated)
    }

    /// Access the normalized timestamp.
    #[must_use]
    pub fn value(self) -> DateTime<Utc> {
        self.0
    }
}

impl Serialize for MillisecondTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

impl<'de> Deserialize<'de> for MillisecondTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let parsed = DateTime::parse_from_rfc3339(&raw)
            .map_err(|_| serde::de::Error::custom("timestamp must be RFC3339"))?
            .with_timezone(&Utc);
        let value = Self::new(parsed);
        if raw != value.0.to_rfc3339_opts(SecondsFormat::Millis, true) {
            return Err(serde::de::Error::custom(
                "timestamp must be canonical UTC with millisecond precision",
            ));
        }
        Ok(value)
    }
}

/// Runtime tool identifier bounded to 256 UTF-8 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolName(String);

impl ToolName {
    /// Validate and construct a bounded tool identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, FrameError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_TOOL_NAME_BYTES {
            return Err(FrameError::InvalidFrame(
                "tool name must contain 1..=256 UTF-8 bytes".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the admitted tool identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?)
            .map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

/// Immutable identity injected by the host during handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub package_id: String,
    pub package_version: String,
    pub package_digest: String,
    pub service_id: String,
    pub activation_revision: u64,
    pub activation_epoch: Uuid,
    pub replay_floor: Sequence,
}

/// Negotiated connection limits sent by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceLimits {
    pub max_frame_bytes: u64,
    pub outbound_messages: u64,
    pub outbound_bytes: u64,
    pub heartbeat_interval_ms: u64,
    pub heartbeat_timeout_ms: u64,
}

/// Host's first handshake frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostHello {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: HostHelloFrame,
    pub connection_id: Uuid,
    pub daemon_boot_id: Uuid,
    pub identity: ServiceIdentity,
    pub limits: ServiceLimits,
}

/// Closed lifecycle event vocabulary, including the reserved schema-only kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventKind {
    DaemonStarted,
    SessionStarted,
    TurnStarted,
    PermissionRequested,
    PermissionResolved,
    ToolStarted,
    ToolFinished,
    TurnFinished,
    SessionStopped,
    DaemonStopping,
}

/// Optional boot-local resume cursor offered by a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCursor {
    pub daemon_boot_id: Uuid,
    pub activation_epoch: Uuid,
    pub after_sequence: Sequence,
}

/// Child's handshake response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceHello {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: ServiceHelloFrame,
    pub subscriptions: Vec<LifecycleEventKind>,
    pub resume: Option<ResumeCursor>,
}

impl ServiceHello {
    /// Prove the negotiated subscription is duplicate-free and does not widen
    /// the service manifest's declared event ceiling.
    pub fn validate_subscriptions(
        &self,
        declared: &[LifecycleEventKind],
    ) -> Result<(), FrameError> {
        let mut seen = std::collections::HashSet::with_capacity(self.subscriptions.len());
        for subscription in &self.subscriptions {
            if !seen.insert(*subscription) {
                return Err(FrameError::InvalidFrame(
                    "subscriptions must not contain duplicates".to_owned(),
                ));
            }
            if !declared.contains(subscription) {
                return Err(FrameError::InvalidFrame(
                    "subscription exceeds declared event ceiling".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Replay mode selected by v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    BootLocal,
}

/// Host readiness frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: ReadyFrame,
    pub subscriptions: Vec<LifecycleEventKind>,
    pub replay: ReplayMode,
    pub activation_epoch: Uuid,
    pub replay_floor: Sequence,
}

/// Lifecycle correlation scope. Missing identities remain explicit JSON nulls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleScope {
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    pub tool_call_id: Option<Uuid>,
    pub permission_id: Option<Uuid>,
}

/// Tool completion outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Success,
    Error,
    Cancelled,
}

/// Permission terminal outcome. `allow_session` intentionally maps to `allowed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    Cancelled,
}

/// Turn lifecycle terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

/// Reserved session-stop reason. No Stage A producer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStopReason {
    ExplicitStop,
}

/// Graceful daemon-stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStopReason {
    GracefulShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyMetadata {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStartedMetadata {
    pub daemon_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestedMetadata {
    pub tool_name: ToolName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResolvedMetadata {
    pub outcome: PermissionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolStartedMetadata {
    pub tool_name: ToolName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFinishedMetadata {
    pub tool_name: ToolName,
    pub outcome: ToolOutcome,
    pub duration_ms: u64,
    pub output_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnFinishedMetadata {
    pub outcome: TurnOutcome,
    pub duration_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStoppedMetadata {
    pub reason: SessionStopReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStoppingMetadata {
    pub reason: DaemonStopReason,
}

/// Closed metadata union. It has no arbitrary JSON escape hatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LifecycleMetadata {
    DaemonStarted(DaemonStartedMetadata),
    PermissionRequested(PermissionRequestedMetadata),
    PermissionResolved(PermissionResolvedMetadata),
    ToolStarted(ToolStartedMetadata),
    ToolFinished(ToolFinishedMetadata),
    TurnFinished(TurnFinishedMetadata),
    SessionStopped(SessionStoppedMetadata),
    DaemonStopping(DaemonStoppingMetadata),
    Empty(EmptyMetadata),
}

/// Metadata-only lifecycle event frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: EventFrame,
    pub daemon_boot_id: Uuid,
    pub sequence: Sequence,
    pub event_id: Uuid,
    pub occurred_at: MillisecondTimestamp,
    pub kind: LifecycleEventKind,
    pub scope: LifecycleScope,
    pub metadata: LifecycleMetadata,
}

impl LifecycleEvent {
    fn metadata_matches_kind(&self) -> bool {
        matches!(
            (&self.kind, &self.metadata),
            (
                LifecycleEventKind::DaemonStarted,
                LifecycleMetadata::DaemonStarted(_)
            ) | (
                LifecycleEventKind::SessionStarted | LifecycleEventKind::TurnStarted,
                LifecycleMetadata::Empty(_)
            ) | (
                LifecycleEventKind::PermissionRequested,
                LifecycleMetadata::PermissionRequested(_)
            ) | (
                LifecycleEventKind::PermissionResolved,
                LifecycleMetadata::PermissionResolved(_)
            ) | (
                LifecycleEventKind::ToolStarted,
                LifecycleMetadata::ToolStarted(_)
            ) | (
                LifecycleEventKind::ToolFinished,
                LifecycleMetadata::ToolFinished(_)
            ) | (
                LifecycleEventKind::TurnFinished,
                LifecycleMetadata::TurnFinished(_)
            ) | (
                LifecycleEventKind::SessionStopped,
                LifecycleMetadata::SessionStopped(_)
            ) | (
                LifecycleEventKind::DaemonStopping,
                LifecycleMetadata::DaemonStopping(_)
            )
        )
    }
}

impl Serialize for LifecycleEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if !self.metadata_matches_kind() {
            return Err(serde::ser::Error::custom(
                "metadata does not match lifecycle kind",
            ));
        }
        if self.event_id.get_version_num() != 4 {
            return Err(serde::ser::Error::custom("event_id must be a UUID v4"));
        }
        let mut frame = serializer.serialize_struct("LifecycleEvent", 10)?;
        frame.serialize_field("protocol", &self.protocol)?;
        frame.serialize_field("version", &self.version)?;
        frame.serialize_field("frame", &self.frame)?;
        frame.serialize_field("daemon_boot_id", &self.daemon_boot_id)?;
        frame.serialize_field("sequence", &self.sequence)?;
        frame.serialize_field("event_id", &self.event_id)?;
        frame.serialize_field("occurred_at", &self.occurred_at)?;
        frame.serialize_field("kind", &self.kind)?;
        frame.serialize_field("scope", &self.scope)?;
        frame.serialize_field("metadata", &self.metadata)?;
        frame.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLifecycleEvent {
    protocol: ProtocolName,
    version: ProtocolV1,
    frame: EventFrame,
    daemon_boot_id: Uuid,
    sequence: Sequence,
    #[serde(deserialize_with = "deserialize_uuid_v4")]
    event_id: Uuid,
    occurred_at: MillisecondTimestamp,
    kind: LifecycleEventKind,
    scope: LifecycleScope,
    metadata: serde_json::Value,
}

fn deserialize_uuid_v4<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Uuid::deserialize(deserializer)?;
    if value.get_version_num() == 4 {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("event_id must be a UUID v4"))
    }
}

impl<'de> Deserialize<'de> for LifecycleEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawLifecycleEvent::deserialize(deserializer)?;
        let metadata = match raw.kind {
            LifecycleEventKind::DaemonStarted => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::DaemonStarted)
            }
            LifecycleEventKind::SessionStarted | LifecycleEventKind::TurnStarted => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::Empty)
            }
            LifecycleEventKind::PermissionRequested => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::PermissionRequested)
            }
            LifecycleEventKind::PermissionResolved => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::PermissionResolved)
            }
            LifecycleEventKind::ToolStarted => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::ToolStarted)
            }
            LifecycleEventKind::ToolFinished => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::ToolFinished)
            }
            LifecycleEventKind::TurnFinished => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::TurnFinished)
            }
            LifecycleEventKind::SessionStopped => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::SessionStopped)
            }
            LifecycleEventKind::DaemonStopping => {
                serde_json::from_value(raw.metadata).map(LifecycleMetadata::DaemonStopping)
            }
        }
        .map_err(|error| serde::de::Error::custom(error.to_string()))?;
        Ok(Self {
            protocol: raw.protocol,
            version: raw.version,
            frame: raw.frame,
            daemon_boot_id: raw.daemon_boot_id,
            sequence: raw.sequence,
            event_id: raw.event_id,
            occurred_at: raw.occurred_at,
            kind: raw.kind,
            scope: raw.scope,
            metadata,
        })
    }
}

/// Child acknowledgement frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: AckFrame,
    pub sequence: Sequence,
}

/// Child pong frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pong {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: PongFrame,
    pub nonce: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatusState {
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatusCode {
    ExternalUnavailable,
    ConfigurationMissing,
    RateLimited,
    Unknown,
}

/// Optional child status frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: StatusFrame,
    pub state: ServiceStatusState,
    pub code: ServiceStatusCode,
}

/// Child graceful-shutdown completion frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownComplete {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: ShutdownCompleteFrame,
}

/// Host lag notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lag {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: LagFrame,
    pub first_lost: Sequence,
    pub last_lost: Sequence,
    pub lost_count: u64,
    pub replay_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    BootChanged,
    ActivationChanged,
    RetentionExceeded,
    InvalidCursor,
}

/// Host reset notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reset {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: ResetFrame,
    pub reason: ResetReason,
    pub oldest_available: Option<Sequence>,
    pub latest_available: Option<Sequence>,
}

/// Host heartbeat ping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ping {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: PingFrame,
    pub nonce: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Disabled,
    DaemonStopping,
    Reconfigure,
    Unhealthy,
}

/// Host graceful-shutdown request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {
    pub protocol: ProtocolName,
    pub version: ProtocolV1,
    pub frame: ShutdownFrame,
    pub reason: ShutdownReason,
}
