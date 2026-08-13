//! Card envelopes and deterministic board projection for Ocean rooms.
//!
//! A Kanban board is not a new storage engine: it is a **projection over an
//! existing room transcript**. A card event is an ordinary [room message] whose
//! body carries a tagged JSON envelope; every other message in the room stays
//! ordinary chat. Folding a room's messages yields the board.
//!
//! This crate is pure and I/O-free: encoding, decoding, and folding only. It
//! owns no transport, no persistence, and no UI.
//!
//! # Why cards are keyed by `card_id`, not by thread structure
//!
//! Room messages carry `thread_parent_seq`, so modelling a card as a thread
//! root is the obvious first idea. It does not survive federation:
//! `ocean-store`'s federated append writes `thread_parent_seq` and `session_id`
//! as `NULL`, and the federated wire payload is a closed struct of
//! `client_event_id` / `author_member_id` / `body` / `mention_member_ids`. A
//! card built on thread parentage works locally and silently flattens into
//! unrelated messages on every other seat.
//!
//! `body` is a plain string that federates untouched, so the envelope travels
//! inside it and every card event names its own `card_id`. The same projection
//! code therefore runs identically on a local-only room and a federated one,
//! and no wire contract has to change.
//!
//! # Ordering
//!
//! Local `seq` is assigned per-daemon (`MAX(seq) + 1` in that daemon's store),
//! so the *same* logical event has different local sequence numbers on
//! different seats. It cannot be the clock for a shared board. The federated
//! `global_sequence` is Bedrock-assigned and documented as the confirmed
//! display order, so it is authoritative whenever present. See [`EventClock`].
//!
//! [room message]: https://docs.rs/ocean-core

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Tag identifying a message body as a board card event.
///
/// Present as the `kind` field of the envelope object. A body that is not JSON,
/// not an object, or carries a different `kind` is ordinary room chat and is
/// skipped by the projection without inspection.
pub const ENVELOPE_KIND: &str = "ocean.board.card";

/// Envelope schema version understood by this crate.
pub const ENVELOPE_VERSION: u32 = 1;

/// Columns every board starts with, in display order.
///
/// Encountered columns outside this set are preserved and appended rather than
/// dropped, so a seat running a newer build cannot make another seat's cards
/// disappear.
pub const DEFAULT_COLUMNS: &[&str] = &["backlog", "next", "doing", "review", "done"];

/// Maximum characters accepted by [`CardEnvelope::encode`] for a card title.
pub const MAX_TITLE_CHARS: usize = 200;

/// Maximum characters accepted by [`CardEnvelope::encode`] for a comment.
pub const MAX_COMMENT_CHARS: usize = 4000;

// ── ordering ──────────────────────────────────────────────────────────

/// The clock a board event is ordered by.
///
/// `Confirmed` carries Bedrock's global ledger sequence: identical on every
/// seat, so all seats fold to the same board. `Pending` carries a local room
/// `seq` for an event that is either local-only or not yet confirmed by
/// Bedrock.
///
/// Every `Pending` event sorts after every `Confirmed` one. That is deliberate:
/// a card you just moved should appear moved immediately, and settle into its
/// true position once confirmation assigns it a global sequence. It matches the
/// optimistic outbox the room UI already renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventClock {
    /// Bedrock global ledger sequence — the confirmed display order.
    Confirmed(u64),
    /// Local room `seq`, pending confirmation (or a local-only room).
    Pending(u64),
}

// ── envelope ──────────────────────────────────────────────────────────

/// What a card event does.
///
/// Serialized with an internal `op` tag. An unrecognized `op` decodes to
/// [`Decoded::Unsupported`] rather than failing the fold, so a seat on an older
/// build degrades to ignoring newer operations instead of showing no board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CardOp {
    /// Bring a card into existence with a title and starting column.
    Create { title: String, column: String },
    /// Move the card to a column.
    Move { column: String },
    /// Set or clear the assignee (a room participant id).
    Assign { assignee: Option<String> },
    /// Change the title.
    Retitle { title: String },
    /// Append a comment.
    Comment { text: String },
    /// Archive the card. Its column is preserved.
    Close,
    /// Un-archive a previously closed card.
    Reopen,
}

/// A single card event, as carried in a room message body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardEnvelope {
    /// Always [`ENVELOPE_KIND`]; the discriminator against ordinary chat.
    pub kind: String,
    /// Envelope schema version.
    pub v: u32,
    /// Stable, client-generated card identity (e.g. a UUIDv4).
    ///
    /// This is the join key for the whole projection. It must be unique and it
    /// must never be reused for a different card.
    pub card_id: String,
    /// The operation.
    #[serde(flatten)]
    pub op: CardOp,
}

/// Why [`CardEnvelope::encode`] refused to build a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// `card_id` was empty or blank.
    EmptyCardId,
    /// A title was empty or blank.
    EmptyTitle,
    /// A column name was empty or blank.
    EmptyColumn,
    /// A comment was empty or blank.
    EmptyComment,
    /// A field exceeded its documented maximum.
    TooLong { field: &'static str, max: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCardId => f.write_str("card_id must not be blank"),
            Self::EmptyTitle => f.write_str("title must not be blank"),
            Self::EmptyColumn => f.write_str("column must not be blank"),
            Self::EmptyComment => f.write_str("comment must not be blank"),
            Self::TooLong { field, max } => {
                write!(f, "{field} exceeds the {max}-character maximum")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

impl CardEnvelope {
    /// Build an envelope at the current schema version.
    pub fn new(card_id: impl Into<String>, op: CardOp) -> Self {
        Self {
            kind: ENVELOPE_KIND.to_string(),
            v: ENVELOPE_VERSION,
            card_id: card_id.into(),
            op,
        }
    }

    /// Validate and serialize into a room message body.
    ///
    /// Validation happens here rather than at decode time on purpose: a
    /// malformed event that already reached the transcript is history, and
    /// dropping it during a fold would make seats disagree.
    pub fn encode(&self) -> Result<String, EncodeError> {
        if self.card_id.trim().is_empty() {
            return Err(EncodeError::EmptyCardId);
        }
        match &self.op {
            CardOp::Create { title, column } => {
                check_text(title, "title", MAX_TITLE_CHARS, EncodeError::EmptyTitle)?;
                check_text(column, "column", MAX_TITLE_CHARS, EncodeError::EmptyColumn)?;
            }
            CardOp::Retitle { title } => {
                check_text(title, "title", MAX_TITLE_CHARS, EncodeError::EmptyTitle)?;
            }
            CardOp::Move { column } => {
                check_text(column, "column", MAX_TITLE_CHARS, EncodeError::EmptyColumn)?;
            }
            CardOp::Comment { text } => {
                check_text(
                    text,
                    "comment",
                    MAX_COMMENT_CHARS,
                    EncodeError::EmptyComment,
                )?;
            }
            CardOp::Assign { .. } | CardOp::Close | CardOp::Reopen => {}
        }
        Ok(serde_json::to_string(self).expect("card envelope is always serializable"))
    }
}

fn check_text(
    value: &str,
    field: &'static str,
    max: usize,
    empty: EncodeError,
) -> Result<(), EncodeError> {
    if value.trim().is_empty() {
        return Err(empty);
    }
    if value.chars().count() > max {
        return Err(EncodeError::TooLong { field, max });
    }
    Ok(())
}

/// The result of inspecting one room message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// Ordinary room chat — not a card event.
    NotCardEvent,
    /// A card event this build understands.
    Card(Box<CardEnvelope>),
    /// A tagged card event this build cannot apply, typically from a seat on a
    /// newer schema. Counted by the projection so a UI can say so out loud
    /// instead of quietly rendering a board that is missing changes.
    Unsupported,
}

/// Classify a room message body.
///
/// Cheap and total: any body that is not a JSON object tagged with
/// [`ENVELOPE_KIND`] is [`Decoded::NotCardEvent`], so ordinary conversation in
/// a board room costs one failed parse and nothing more.
pub fn decode(body: &str) -> Decoded {
    // Fast reject before touching the JSON parser: card bodies are always
    // objects and always mention the tag.
    let trimmed = body.trim_start();
    if !trimmed.starts_with('{') || !body.contains(ENVELOPE_KIND) {
        return Decoded::NotCardEvent;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Decoded::NotCardEvent;
    };
    if value.get("kind").and_then(serde_json::Value::as_str) != Some(ENVELOPE_KIND) {
        return Decoded::NotCardEvent;
    }
    // Tagged as ours from here on: anything we cannot apply is a real gap in
    // the board, never silent chat.
    if value.get("v").and_then(serde_json::Value::as_u64) != Some(u64::from(ENVELOPE_VERSION)) {
        return Decoded::Unsupported;
    }
    match serde_json::from_value::<CardEnvelope>(value) {
        Ok(envelope) if !envelope.card_id.trim().is_empty() => Decoded::Card(Box::new(envelope)),
        _ => Decoded::Unsupported,
    }
}

// ── projection ────────────────────────────────────────────────────────

/// One room message offered to the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardEvent<'a> {
    /// Ordering clock for this event.
    pub clock: EventClock,
    /// Participant id of the author.
    pub author_id: &'a str,
    /// Raw message body.
    pub body: &'a str,
}

/// A comment on a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardComment {
    pub clock: EventClock,
    pub author_id: String,
    pub text: String,
}

/// A card, as folded from the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    /// Empty until a `create` event arrives — a card observed only through a
    /// later event still exists, so nothing is lost to out-of-order delivery.
    pub title: String,
    pub column: String,
    pub assignee: Option<String>,
    pub closed: bool,
    /// Author of the earliest event seen for this card.
    pub created_by: String,
    /// Clock of the earliest event seen for this card.
    pub created_at: EventClock,
    /// Clock of the latest event seen for this card.
    pub updated_at: EventClock,
    pub comments: Vec<CardComment>,
    /// Set when a `create` event has actually been folded in.
    pub created: bool,
    // Per-field write clocks. Last-writer-wins is resolved per field rather
    // than per card, so the fold does not depend on the order events arrive.
    #[serde(skip)]
    title_clock: Option<EventClock>,
    #[serde(skip)]
    column_clock: Option<EventClock>,
    #[serde(skip)]
    assignee_clock: Option<EventClock>,
    #[serde(skip)]
    closed_clock: Option<EventClock>,
}

/// A folded board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// Column names in display order: [`DEFAULT_COLUMNS`] first, then any
    /// others encountered, sorted for determinism.
    pub columns: Vec<String>,
    /// Cards by `card_id`. A `BTreeMap` so iteration order is stable.
    pub cards: BTreeMap<String, Card>,
    /// Count of tagged card events this build could not apply.
    pub unsupported_events: u64,
}

impl Board {
    /// Cards in one column, closed cards excluded, ordered by creation clock.
    pub fn column(&self, column: &str) -> Vec<&Card> {
        let mut found: Vec<&Card> = self
            .cards
            .values()
            .filter(|card| !card.closed && card.column == column)
            .collect();
        found.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        found
    }

    /// All archived cards, ordered by creation clock.
    pub fn closed(&self) -> Vec<&Card> {
        let mut found: Vec<&Card> = self.cards.values().filter(|card| card.closed).collect();
        found.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        found
    }
}

/// Fold room messages into a board.
///
/// The result does not depend on the order events are supplied: every field
/// resolves last-writer-wins against [`EventClock`], so replaying a prefix and
/// then the remainder gives the same board as folding the whole transcript at
/// once. That is what lets a seat apply a live tail without re-reading history,
/// and lets federated events arrive interleaved with local ones.
pub fn project<'a>(events: impl IntoIterator<Item = BoardEvent<'a>>) -> Board {
    let mut cards: BTreeMap<String, Card> = BTreeMap::new();
    let mut unsupported_events = 0u64;
    let mut extra_columns: BTreeSet<String> = BTreeSet::new();

    for event in events {
        let envelope = match decode(event.body) {
            Decoded::Card(envelope) => envelope,
            Decoded::Unsupported => {
                unsupported_events += 1;
                continue;
            }
            Decoded::NotCardEvent => continue,
        };

        let card = cards
            .entry(envelope.card_id.clone())
            .or_insert_with(|| Card {
                id: envelope.card_id.clone(),
                title: String::new(),
                column: DEFAULT_COLUMNS[0].to_string(),
                assignee: None,
                closed: false,
                created_by: event.author_id.to_string(),
                created_at: event.clock,
                updated_at: event.clock,
                comments: Vec::new(),
                created: false,
                title_clock: None,
                column_clock: None,
                assignee_clock: None,
                closed_clock: None,
            });

        // Earliest-event attribution, resolved independently of arrival order.
        if event.clock < card.created_at {
            card.created_at = event.clock;
            card.created_by = event.author_id.to_string();
        }
        if event.clock > card.updated_at {
            card.updated_at = event.clock;
        }

        match envelope.op {
            CardOp::Create { title, column } => {
                card.created = true;
                if wins(&mut card.title_clock, event.clock) {
                    card.title = title;
                }
                if wins(&mut card.column_clock, event.clock) {
                    note_column(&column, &mut extra_columns);
                    card.column = column;
                }
            }
            CardOp::Retitle { title } => {
                if wins(&mut card.title_clock, event.clock) {
                    card.title = title;
                }
            }
            CardOp::Move { column } => {
                if wins(&mut card.column_clock, event.clock) {
                    note_column(&column, &mut extra_columns);
                    card.column = column;
                }
            }
            CardOp::Assign { assignee } => {
                if wins(&mut card.assignee_clock, event.clock) {
                    card.assignee = assignee.filter(|a| !a.trim().is_empty());
                }
            }
            CardOp::Close => {
                if wins(&mut card.closed_clock, event.clock) {
                    card.closed = true;
                }
            }
            CardOp::Reopen => {
                if wins(&mut card.closed_clock, event.clock) {
                    card.closed = false;
                }
            }
            CardOp::Comment { text } => {
                let comment = CardComment {
                    clock: event.clock,
                    author_id: event.author_id.to_string(),
                    text,
                };
                // Idempotent: the same comment replayed twice is one comment.
                if !card.comments.contains(&comment) {
                    card.comments.push(comment);
                }
            }
        }
    }

    for card in cards.values_mut() {
        card.comments
            .sort_by(|a, b| a.clock.cmp(&b.clock).then_with(|| a.text.cmp(&b.text)));
    }

    let mut columns: Vec<String> = DEFAULT_COLUMNS.iter().map(|c| (*c).to_string()).collect();
    columns.extend(extra_columns);

    Board {
        columns,
        cards,
        unsupported_events,
    }
}

/// Record a column that is not one of [`DEFAULT_COLUMNS`].
fn note_column(column: &str, extra: &mut BTreeSet<String>) {
    if !DEFAULT_COLUMNS.contains(&column) {
        extra.insert(column.to_string());
    }
}

/// Last-writer-wins on a single field. Strictly greater, so equal clocks keep
/// the first write and the fold stays deterministic.
fn wins(slot: &mut Option<EventClock>, clock: EventClock) -> bool {
    match slot {
        Some(existing) if *existing >= clock => false,
        _ => {
            *slot = Some(clock);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(card_id: &str, op: CardOp) -> String {
        CardEnvelope::new(card_id, op).encode().unwrap()
    }

    fn ev<'a>(seq: u64, author: &'a str, body: &'a str) -> BoardEvent<'a> {
        BoardEvent {
            clock: EventClock::Confirmed(seq),
            author_id: author,
            body,
        }
    }

    #[test]
    fn ordinary_chat_is_not_a_card_event() {
        assert_eq!(decode("hey, standup in 5"), Decoded::NotCardEvent);
        assert_eq!(decode(""), Decoded::NotCardEvent);
        assert_eq!(decode("{ not json"), Decoded::NotCardEvent);
        assert_eq!(
            decode(r#"{"kind":"something.else"}"#),
            Decoded::NotCardEvent
        );
    }

    #[test]
    fn chat_that_merely_mentions_the_tag_is_still_chat() {
        // The fast-path check looks for the tag substring; a sentence quoting
        // it must not be promoted to a card event.
        assert_eq!(
            decode("we should use ocean.board.card for this"),
            Decoded::NotCardEvent
        );
    }

    #[test]
    fn round_trips_an_envelope() {
        let encoded = body(
            "card-1",
            CardOp::Create {
                title: "Ship the board".into(),
                column: "backlog".into(),
            },
        );
        let Decoded::Card(envelope) = decode(&encoded) else {
            panic!("expected a card event");
        };
        assert_eq!(envelope.card_id, "card-1");
        assert_eq!(
            envelope.op,
            CardOp::Create {
                title: "Ship the board".into(),
                column: "backlog".into()
            }
        );
    }

    #[test]
    fn newer_schema_version_is_unsupported_not_chat() {
        let newer = format!(
            r#"{{"kind":"{ENVELOPE_KIND}","v":99,"card_id":"c1","op":"create","title":"x","column":"backlog"}}"#
        );
        assert_eq!(decode(&newer), Decoded::Unsupported);
    }

    #[test]
    fn unknown_op_is_unsupported_not_chat() {
        let unknown = format!(
            r#"{{"kind":"{ENVELOPE_KIND}","v":1,"card_id":"c1","op":"teleport","column":"backlog"}}"#
        );
        assert_eq!(decode(&unknown), Decoded::Unsupported);
    }

    #[test]
    fn unsupported_events_are_counted_so_a_ui_can_warn() {
        let newer = format!(r#"{{"kind":"{ENVELOPE_KIND}","v":99,"card_id":"c1","op":"create"}}"#);
        let board = project(vec![ev(1, "ec", &newer)]);
        assert_eq!(board.unsupported_events, 1);
        assert!(board.cards.is_empty());
    }

    #[test]
    fn encode_rejects_blank_and_oversized_fields() {
        assert_eq!(
            CardEnvelope::new("", CardOp::Close).encode(),
            Err(EncodeError::EmptyCardId)
        );
        assert_eq!(
            CardEnvelope::new(
                "c1",
                CardOp::Retitle {
                    title: "   ".into()
                }
            )
            .encode(),
            Err(EncodeError::EmptyTitle)
        );
        assert_eq!(
            CardEnvelope::new(
                "c1",
                CardOp::Comment {
                    text: "x".repeat(MAX_COMMENT_CHARS + 1)
                }
            )
            .encode(),
            Err(EncodeError::TooLong {
                field: "comment",
                max: MAX_COMMENT_CHARS
            })
        );
    }

    #[test]
    fn folds_a_card_through_its_lifecycle() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "Wire the SSE endpoint".into(),
                column: "backlog".into(),
            },
        );
        let assign = body(
            "c1",
            CardOp::Assign {
                assignee: Some("nia".into()),
            },
        );
        let mv = body(
            "c1",
            CardOp::Move {
                column: "doing".into(),
            },
        );
        let board = project(vec![
            ev(1, "ec", &create),
            ev(2, "ec", &assign),
            ev(3, "nia", &mv),
        ]);

        let card = &board.cards["c1"];
        assert_eq!(card.title, "Wire the SSE endpoint");
        assert_eq!(card.column, "doing");
        assert_eq!(card.assignee.as_deref(), Some("nia"));
        assert!(card.created && !card.closed);
        assert_eq!(card.created_by, "ec");
        assert_eq!(card.updated_at, EventClock::Confirmed(3));
        assert_eq!(board.column("doing").len(), 1);
        assert!(board.column("backlog").is_empty());
    }

    #[test]
    fn chat_interleaved_with_cards_is_ignored() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "next".into(),
            },
        );
        let board = project(vec![
            ev(1, "ec", "morning"),
            ev(2, "ec", &create),
            ev(3, "nia", "nice"),
        ]);
        assert_eq!(board.cards.len(), 1);
        assert_eq!(board.unsupported_events, 0);
    }

    #[test]
    fn fold_is_order_independent() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "backlog".into(),
            },
        );
        let mv = body(
            "c1",
            CardOp::Move {
                column: "review".into(),
            },
        );
        let assign = body(
            "c1",
            CardOp::Assign {
                assignee: Some("ec".into()),
            },
        );

        let forward = project(vec![
            ev(1, "ec", &create),
            ev(2, "ec", &mv),
            ev(3, "ec", &assign),
        ]);
        let reversed = project(vec![
            ev(3, "ec", &assign),
            ev(2, "ec", &mv),
            ev(1, "ec", &create),
        ]);
        let shuffled = project(vec![
            ev(2, "ec", &mv),
            ev(1, "ec", &create),
            ev(3, "ec", &assign),
        ]);

        assert_eq!(forward, reversed);
        assert_eq!(forward, shuffled);
        assert_eq!(forward.cards["c1"].column, "review");
    }

    #[test]
    fn folding_a_prefix_then_the_rest_matches_folding_everything() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "backlog".into(),
            },
        );
        let mv = body(
            "c1",
            CardOp::Move {
                column: "doing".into(),
            },
        );
        let all = vec![ev(1, "ec", &create), ev(2, "ec", &mv)];
        let whole = project(all.clone());

        // Same events, delivered as a hydrate plus a live tail.
        let mut staged = all[..1].to_vec();
        staged.extend_from_slice(&all[1..]);
        assert_eq!(project(staged), whole);
    }

    #[test]
    fn replay_is_idempotent() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "backlog".into(),
            },
        );
        let comment = body(
            "c1",
            CardOp::Comment {
                text: "on it".into(),
            },
        );
        let once = project(vec![ev(1, "ec", &create), ev(2, "ec", &comment)]);
        let twice = project(vec![
            ev(1, "ec", &create),
            ev(2, "ec", &comment),
            ev(1, "ec", &create),
            ev(2, "ec", &comment),
        ]);
        assert_eq!(once, twice);
        assert_eq!(once.cards["c1"].comments.len(), 1);
    }

    #[test]
    fn a_card_observed_before_its_create_is_not_lost() {
        // Out-of-order delivery: the move lands first.
        let mv = body(
            "c1",
            CardOp::Move {
                column: "doing".into(),
            },
        );
        let create = body(
            "c1",
            CardOp::Create {
                title: "Late create".into(),
                column: "backlog".into(),
            },
        );
        let board = project(vec![ev(5, "nia", &mv), ev(1, "ec", &create)]);
        let card = &board.cards["c1"];
        assert_eq!(card.title, "Late create");
        // The later move still wins the column.
        assert_eq!(card.column, "doing");
        assert!(card.created);
        assert_eq!(card.created_by, "ec");
        assert_eq!(card.created_at, EventClock::Confirmed(1));
    }

    #[test]
    fn confirmed_events_outrank_pending_ones() {
        // A pending local move must win over an older confirmed one, because
        // it is the newer intent and has simply not been confirmed yet.
        let confirmed_move = body(
            "c1",
            CardOp::Move {
                column: "review".into(),
            },
        );
        let pending_move = body(
            "c1",
            CardOp::Move {
                column: "done".into(),
            },
        );
        let board = project(vec![
            BoardEvent {
                clock: EventClock::Confirmed(99),
                author_id: "nia",
                body: &confirmed_move,
            },
            BoardEvent {
                clock: EventClock::Pending(1),
                author_id: "ec",
                body: &pending_move,
            },
        ]);
        assert_eq!(board.cards["c1"].column, "done");
    }

    #[test]
    fn close_and_reopen_resolve_by_clock() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "done".into(),
            },
        );
        let close = body("c1", CardOp::Close);
        let reopen = body("c1", CardOp::Reopen);

        let closed = project(vec![ev(1, "ec", &create), ev(2, "ec", &close)]);
        assert!(closed.cards["c1"].closed);
        assert_eq!(closed.closed().len(), 1);
        assert!(closed.column("done").is_empty());

        let reopened = project(vec![
            ev(1, "ec", &create),
            ev(2, "ec", &close),
            ev(3, "nia", &reopen),
        ]);
        assert!(!reopened.cards["c1"].closed);
        assert_eq!(reopened.column("done").len(), 1);
    }

    #[test]
    fn unassign_clears_the_assignee() {
        let assign = body(
            "c1",
            CardOp::Assign {
                assignee: Some("ec".into()),
            },
        );
        let unassign = body("c1", CardOp::Assign { assignee: None });
        let board = project(vec![ev(1, "ec", &assign), ev(2, "ec", &unassign)]);
        assert_eq!(board.cards["c1"].assignee, None);
    }

    #[test]
    fn unknown_columns_are_preserved_not_dropped() {
        let create = body(
            "c1",
            CardOp::Create {
                title: "T".into(),
                column: "blocked".into(),
            },
        );
        let board = project(vec![ev(1, "ec", &create)]);
        assert!(board.columns.contains(&"blocked".to_string()));
        // Defaults keep their order and stay ahead of the newcomer.
        assert_eq!(&board.columns[..DEFAULT_COLUMNS.len()], DEFAULT_COLUMNS);
        assert_eq!(board.column("blocked").len(), 1);
    }

    #[test]
    fn separate_cards_stay_separate() {
        let a = body(
            "a",
            CardOp::Create {
                title: "A".into(),
                column: "backlog".into(),
            },
        );
        let b = body(
            "b",
            CardOp::Create {
                title: "B".into(),
                column: "doing".into(),
            },
        );
        let board = project(vec![ev(1, "ec", &a), ev(2, "nia", &b)]);
        assert_eq!(board.cards.len(), 2);
        assert_eq!(board.cards["a"].title, "A");
        assert_eq!(board.cards["b"].title, "B");
    }

    #[test]
    fn empty_transcript_yields_an_empty_board() {
        let board = project(Vec::<BoardEvent<'_>>::new());
        assert!(board.cards.is_empty());
        assert_eq!(board.unsupported_events, 0);
        assert_eq!(board.columns, DEFAULT_COLUMNS);
    }
}
