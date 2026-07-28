use axum::{http::StatusCode, Json};
use ocean_agent_sdk::buddy::{
    BuddyAttachmentTarget, BuddyCard, BuddyEvent, BuddyEventResponse, BuddyEventState,
};
use serde_json::{json, Value};
use uuid::Uuid;

/// Accept the first Ocean Buddy event delivered by the iPhone sensor.
///
/// The vertical slice is intentionally narrow: Watch approval stays between
/// Watch and iPhone, and the daemon accepts only the mocked `attached` event.
/// No image bytes are uploaded or persisted here.
pub(super) async fn ocean_buddy_event(
    Json(event): Json<BuddyEvent>,
) -> Result<Json<BuddyEventResponse>, (StatusCode, Json<Value>)> {
    if event.state != BuddyEventState::Attached {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "accepted": false,
                "error": "the first backend slice accepts only attached events"
            })),
        ));
    }

    if event.target != Some(BuddyAttachmentTarget::CurrentOceanContext)
        || event.action.is_some()
        || event.card.is_some()
        || event.failure.is_some()
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "accepted": false,
                "error": "attached event must declare only the current Ocean context target and attachment metadata"
            })),
        ));
    }

    let Some(attachment) = event.attachment else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "accepted": false,
                "error": "attached event requires attachment metadata"
            })),
        ));
    };

    if !attachment.mock_capture
        || attachment.byte_count != 0
        || attachment.mime_type != "image/jpeg"
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "accepted": false,
                "error": "real photo capture is not implemented"
            })),
        ));
    }

    Ok(Json(BuddyEventResponse {
        accepted: true,
        received_event_id: event.event_id,
        card: BuddyCard::photo_attached_result(Uuid::new_v4()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ocean_agent_sdk::buddy::{
        BuddyAttachment, BuddyAttachmentKind, BuddyAttachmentTarget, BuddyComponentKind,
    };

    fn attached_event(mock_capture: bool) -> BuddyEvent {
        BuddyEvent {
            event_id: Uuid::new_v4(),
            flow_id: Uuid::new_v4(),
            causation_id: Some(Uuid::new_v4()),
            state: BuddyEventState::Attached,
            occurred_at: Utc::now(),
            action: None,
            attachment: Some(BuddyAttachment {
                id: Uuid::new_v4(),
                kind: BuddyAttachmentKind::Photo,
                mime_type: "image/jpeg".into(),
                filename: "ocean-buddy-mock.jpg".into(),
                byte_count: 0,
                mock_capture,
            }),
            target: Some(BuddyAttachmentTarget::CurrentOceanContext),
            card: None,
            failure: None,
        }
    }

    #[tokio::test]
    async fn mocked_attached_event_returns_a_watch_result_card() {
        let event = attached_event(true);
        let event_id = event.event_id;
        let response = ocean_buddy_event(Json(event)).await.unwrap().0;

        assert!(response.accepted);
        assert_eq!(response.received_event_id, event_id);
        assert_eq!(response.card.kind, BuddyComponentKind::ResultCard);
    }

    #[tokio::test]
    async fn non_attached_state_is_rejected_at_the_backend_boundary() {
        let mut event = attached_event(true);
        event.state = BuddyEventState::Approved;
        let error = ocean_buddy_event(Json(event)).await.unwrap_err();

        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn real_capture_is_rejected_at_the_backend_boundary() {
        let error = ocean_buddy_event(Json(attached_event(false)))
            .await
            .unwrap_err();

        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn attached_event_requires_exact_target_and_state_shape() {
        let mut missing_target = attached_event(true);
        missing_target.target = None;
        assert_eq!(
            ocean_buddy_event(Json(missing_target)).await.unwrap_err().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let mut state_smuggling = attached_event(true);
        state_smuggling.card = Some(BuddyCard::photo_attached_result(Uuid::new_v4()));
        assert_eq!(
            ocean_buddy_event(Json(state_smuggling))
                .await
                .unwrap_err()
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
