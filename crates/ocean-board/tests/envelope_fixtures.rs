//! Cross-repository envelope contract fixtures.
//!
//! `ocean-surface` renders the board but does not depend on this crate — it
//! redefines room types locally — so its encoder is a hand-maintained twin of
//! this one. Prose cannot hold two encoders together; these fixtures can.
//!
//! The exact strings below are the contract. The reciprocal test in
//! `ocean-surface` pins the identical strings, so either side drifting turns
//! into a named test failure in that repo rather than cards that silently stop
//! appearing on half the team's screens.
//!
//! Changing a fixture is a schema change: bump `ENVELOPE_VERSION` and land the
//! matching encoder in `ocean-surface` in the same change. Do not "fix" a
//! failure here by editing the expected string.

use ocean_board::{decode, project, BoardEvent, CardEnvelope, CardOp, Decoded, EventClock};

const CARD_ID: &str = "11111111-1111-4111-8111-111111111111";

/// `(case, envelope-producing op, exact encoded body)`.
fn fixtures() -> Vec<(&'static str, CardOp, &'static str)> {
    vec![
        (
            "create",
            CardOp::Create {
                title: "Wire the Bedrock SSE endpoint".into(),
                column: "backlog".into(),
            },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"create","title":"Wire the Bedrock SSE endpoint","column":"backlog"}"#,
        ),
        (
            "move",
            CardOp::Move {
                column: "doing".into(),
            },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"move","column":"doing"}"#,
        ),
        (
            "assign",
            CardOp::Assign {
                assignee: Some("nia".into()),
            },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"assign","assignee":"nia"}"#,
        ),
        (
            "unassign",
            CardOp::Assign { assignee: None },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"assign","assignee":null}"#,
        ),
        (
            "retitle",
            CardOp::Retitle {
                title: "Renamed".into(),
            },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"retitle","title":"Renamed"}"#,
        ),
        (
            "comment",
            CardOp::Comment {
                text: "on it".into(),
            },
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"comment","text":"on it"}"#,
        ),
        (
            "close",
            CardOp::Close,
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"close"}"#,
        ),
        (
            "reopen",
            CardOp::Reopen,
            r#"{"kind":"ocean.board.card","v":1,"card_id":"11111111-1111-4111-8111-111111111111","op":"reopen"}"#,
        ),
    ]
}

#[test]
fn encoder_output_is_byte_exact() {
    for (case, op, expected) in fixtures() {
        let encoded = CardEnvelope::new(CARD_ID, op).encode().unwrap();
        assert_eq!(encoded, expected, "encoding drifted for the `{case}` case");
    }
}

#[test]
fn every_fixture_decodes_back_to_its_op() {
    for (case, op, encoded) in fixtures() {
        let Decoded::Card(envelope) = decode(encoded) else {
            panic!("`{case}` fixture failed to decode as a card event");
        };
        assert_eq!(envelope.card_id, CARD_ID, "`{case}` card_id");
        assert_eq!(envelope.op, op, "`{case}` op");
    }
}

#[test]
fn decoding_does_not_depend_on_field_order() {
    // The encoder is pinned byte-exact, but the wire is JSON: a twin encoder
    // must never be able to break this side by emitting the same fields in a
    // different order.
    let reordered = format!(
        r#"{{"op":"move","column":"doing","card_id":"{CARD_ID}","v":1,"kind":"ocean.board.card"}}"#
    );
    let Decoded::Card(envelope) = decode(&reordered) else {
        panic!("reordered fields must still decode");
    };
    assert_eq!(
        envelope.op,
        CardOp::Move {
            column: "doing".into()
        }
    );
}

#[test]
fn fixtures_fold_into_the_expected_board() {
    // End-to-end over the pinned bytes: the wire contract and the projection
    // agree, not just the encoder.
    let all = fixtures();
    let find = |case: &str| -> &'static str {
        all.iter()
            .find(|(name, _, _)| *name == case)
            .map(|(_, _, body)| *body)
            .expect("fixture case exists")
    };

    let board = project(vec![
        BoardEvent {
            clock: EventClock::Confirmed(1),
            author_id: "ec",
            body: find("create"),
        },
        BoardEvent {
            clock: EventClock::Confirmed(2),
            author_id: "ec",
            body: find("assign"),
        },
        BoardEvent {
            clock: EventClock::Confirmed(3),
            author_id: "nia",
            body: find("move"),
        },
        BoardEvent {
            clock: EventClock::Confirmed(4),
            author_id: "nia",
            body: find("comment"),
        },
    ]);

    let card = &board.cards[CARD_ID];
    assert_eq!(card.title, "Wire the Bedrock SSE endpoint");
    assert_eq!(card.column, "doing");
    assert_eq!(card.assignee.as_deref(), Some("nia"));
    assert_eq!(card.comments.len(), 1);
    assert!(card.created && !card.closed);
    assert_eq!(board.unsupported_events, 0);
}
