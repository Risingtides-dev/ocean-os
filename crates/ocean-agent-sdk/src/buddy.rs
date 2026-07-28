//! Typed wire contract for the first Ocean Buddy vertical slice.
//!
//! Ocean Buddy keeps device behavior client-side: Apple Watch renders and
//! approves a card, iPhone brokers capture, and Ocean OS receives the resulting
//! attachment event. The daemon remains the backend authority.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Card shapes supported by the first Buddy slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyComponentKind {
    ApprovalCard,
    ResultCard,
    ErrorCard,
}

/// Device roles addressable by a Buddy action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyDevice {
    AppleWatch,
    IPhone,
    IPad,
    Ocean,
}

/// Action kinds understood by the first Buddy slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyActionKind {
    PhotoToContext,
}

/// An operator action rendered on a Buddy card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyAction {
    pub id: Uuid,
    pub label: String,
    pub kind: BuddyActionKind,
    pub requires_confirmation: bool,
    pub target_device: BuddyDevice,
}

/// A minimal card that can be projected on Apple Watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyCard {
    pub id: Uuid,
    pub kind: BuddyComponentKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<BuddyAction>,
}

impl BuddyCard {
    /// Build the only approval card in the first vertical slice.
    pub fn attach_photo_approval(id: Uuid, action_id: Uuid) -> Self {
        Self {
            id,
            kind: BuddyComponentKind::ApprovalCard,
            title: "Attach Photo to Current Ocean Context.".into(),
            detail: None,
            actions: vec![BuddyAction {
                id: action_id,
                label: "Approve".into(),
                kind: BuddyActionKind::PhotoToContext,
                requires_confirmation: true,
                target_device: BuddyDevice::IPhone,
            }],
        }
    }

    /// Build the Watch result card returned after Ocean accepts the event.
    pub fn photo_attached_result(id: Uuid) -> Self {
        Self {
            id,
            kind: BuddyComponentKind::ResultCard,
            title: "Photo attached to current Ocean context.".into(),
            detail: Some("Mock iPhone capture accepted by Ocean.".into()),
            actions: Vec::new(),
        }
    }

    /// Build the clear Watch error card for the documented first failure path.
    pub fn phone_unavailable_error(id: Uuid) -> Self {
        Self {
            id,
            kind: BuddyComponentKind::ErrorCard,
            title: "Photo was not attached.".into(),
            detail: Some("iPhone is unavailable. Bring it online and try again.".into()),
            actions: Vec::new(),
        }
    }
}

/// Attachment kinds understood by the first Buddy slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyAttachmentKind {
    Photo,
}

/// The symbolic destination for an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyAttachmentTarget {
    CurrentOceanContext,
}

/// Metadata for a captured attachment.
///
/// The first slice deliberately carries no image bytes. `mock_capture` proves
/// the device-to-daemon event flow without creating a real upload contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyAttachment {
    pub id: Uuid,
    pub kind: BuddyAttachmentKind,
    pub mime_type: String,
    pub filename: String,
    pub byte_count: u64,
    pub mock_capture: bool,
}

/// Ordered lifecycle states for a context-attachment gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyEventState {
    Requested,
    Approved,
    CaptureStarted,
    CaptureCompleted,
    Uploaded,
    Attached,
    Result,
    Failed,
}

/// Stable failure codes rendered as clear Watch error cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuddyFailureCode {
    PhoneUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyFailure {
    pub code: BuddyFailureCode,
    pub message: String,
    pub retryable: bool,
}

/// One event in a Buddy context-attachment flow.
///
/// `state` is the lifecycle discriminator. State-specific data is additive:
/// requested/approved events carry `action`; completed/uploaded/attached events
/// carry `attachment`; attached events carry `target`; result events carry a
/// result `card`; failed events carry both `failure` and an error `card`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyEvent {
    pub event_id: Uuid,
    pub flow_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<Uuid>,
    pub state: BuddyEventState,
    pub occurred_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<BuddyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<BuddyAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BuddyAttachmentTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<BuddyCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<BuddyFailure>,
}

/// Successful acknowledgement returned by the Rust backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuddyEventResponse {
    pub accepted: bool,
    pub received_event_id: Uuid,
    pub card: BuddyCard,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAPPY_PATH: &str = include_str!("../../../docs/examples/ocean-buddy/happy-path.json");
    const PHONE_UNAVAILABLE: &str =
        include_str!("../../../docs/examples/ocean-buddy/phone-unavailable.json");

    #[test]
    fn json_fixtures_cover_happy_and_failure_lifecycles() {
        let happy: Vec<BuddyEvent> = serde_json::from_str(HAPPY_PATH).unwrap();
        assert_eq!(
            happy.iter().map(|event| event.state).collect::<Vec<_>>(),
            vec![
                BuddyEventState::Requested,
                BuddyEventState::Approved,
                BuddyEventState::CaptureStarted,
                BuddyEventState::CaptureCompleted,
                BuddyEventState::Uploaded,
                BuddyEventState::Attached,
                BuddyEventState::Result,
            ]
        );
        assert_eq!(
            happy[0].action.as_ref().unwrap().target_device,
            BuddyDevice::IPhone
        );
        assert!(happy[0].action.as_ref().unwrap().requires_confirmation);

        let failure: Vec<BuddyEvent> = serde_json::from_str(PHONE_UNAVAILABLE).unwrap();
        assert_eq!(failure.last().unwrap().state, BuddyEventState::Failed);
        assert_eq!(
            failure.last().unwrap().failure.as_ref().unwrap().code,
            BuddyFailureCode::PhoneUnavailable
        );
        assert_eq!(
            failure.last().unwrap().card.as_ref().unwrap().kind,
            BuddyComponentKind::ErrorCard
        );
    }

    #[test]
    fn approval_card_has_the_exact_first_slice_contract() {
        let card = BuddyCard::attach_photo_approval(Uuid::nil(), Uuid::nil());
        assert_eq!(card.title, "Attach Photo to Current Ocean Context.");
        assert_eq!(card.actions.len(), 1);
        let action = &card.actions[0];
        assert_eq!(action.label, "Approve");
        assert_eq!(action.kind, BuddyActionKind::PhotoToContext);
        assert!(action.requires_confirmation);
        assert_eq!(action.target_device, BuddyDevice::IPhone);
    }
}
