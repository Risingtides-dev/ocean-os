//! One-shot room-transcript summarization into the room's well-known artifact.
//!
//! A long room is unreadable, and the honest fix is not another wall of chat: it
//! is a durable thing the room OWNS. This module reads a bounded tail of the
//! transcript, runs exactly ONE model turn through the existing
//! [`AgentRuntime::complete_once`] seam, and folds the result into a single
//! well-known `room-summary` artifact — created once, then amended in place
//! forever under the store's compare-and-swap. Re-summarizing a room does not
//! litter it with `summary-1`, `summary-2`, …; it moves one artifact forward.
//!
//! The provider call arrives as a `complete` CLOSURE rather than an
//! `AgentRuntime`, exactly as `advisor.rs` takes it. That is deliberate:
//! `complete_once` resolves credentials through `ProviderEnv::from_process()`,
//! so wiring the runtime in here would make every test of this logic depend on
//! process-global env. With the seam, the whole feature is exercised against an
//! in-memory store and a fake closure — no env, no provider, no `AppState`.

use std::{collections::HashMap, future::Future, time::Duration};

use chrono::{DateTime, Utc};
use ocean_core::{RoomArtifact, RoomArtifactKind, RoomKey, RoomMessage, RoomParticipantKind};
use ocean_store::{RoomStore as _, RoomStoreError, SqliteRoomStore};

use crate::persistent_rooms::{
    read_transcript_page, room_history_text, with_rooms_handle, RoomStoreHandle,
};

/// The well-known artifact id every summarize call writes to. It is a constant,
/// not a caller parameter, because "the room's summary" is a singular thing:
/// repeated calls must amend it, not accumulate near-duplicates.
pub(super) const ROOM_SUMMARY_ARTIFACT_ID: &str = "room-summary";
/// Title used when the artifact is first created. Amends never rewrite it, so a
/// room that later renames the artifact by hand keeps that name.
pub(super) const ROOM_SUMMARY_TITLE: &str = "Room summary";
/// Ceiling on the single provider call. Generous relative to the advisor's 30s
/// because a summary reads far more input, but still bounded: the caller holds a
/// turn permit for the whole handler and must not hold it indefinitely.
pub(super) const ROOM_SUMMARY_TIMEOUT: Duration = Duration::from_secs(45);

/// Everything the summarize pass needs that is not the store or the provider.
pub(super) struct SummarizeInput {
    pub(super) key: RoomKey,
    /// Roster participant the artifact is attributed to. The store requires a
    /// real roster author (`require_roster_author_on`) and rooms are created
    /// with an EMPTY roster, so there is no daemon-authored artifact to fall
    /// back on — the requester owns the write, with model provenance recorded
    /// in the body.
    pub(super) requested_by: String,
    pub(super) limit: Option<usize>,
    /// Pins an explicit window instead of the newest `limit` rows. Omitted is
    /// the ordinary case and the one that matters.
    pub(super) after_seq: Option<u64>,
    pub(super) alias: String,
    pub(super) timeout: Duration,
}

/// What the store write did. `Unchanged` is not a failure: the model looked at
/// the same conversation and said the same thing, which the store correctly
/// refuses to record as a new version.
#[derive(Debug)]
pub(super) enum SummaryWrite {
    Created(RoomArtifact),
    Amended(RoomArtifact),
    Unchanged(RoomArtifact),
}

/// Terminal result of one summarize pass. Every variant except `Store` is a
/// clean, expected answer — a model that returned nothing and a room with no
/// messages are both ordinary, not server faults.
#[derive(Debug)]
pub(super) enum SummarizeOutcome {
    Wrote {
        artifact: RoomArtifact,
        created: bool,
        model: String,
        messages_summarized: usize,
        from_seq: u64,
        to_seq: u64,
        has_more: bool,
        /// The System transcript row the store wrote in the SAME transaction.
        /// The caller publishes a wake for it AFTER the store returns. Boxed
        /// only to keep this variant from dominating the enum's size — every
        /// non-write outcome would otherwise carry a full message's worth of
        /// unused stack.
        message: Box<RoomMessage>,
    },
    Unchanged {
        artifact: RoomArtifact,
    },
    NoMessages,
    EmptySummary,
    /// `requested_by` names an Agent or System participant. An agent's artifact
    /// is authored by the daemon's own convene path, never by a client claiming
    /// its identity — the same rule `room_create_artifact` applies.
    ForgedAuthor,
    ProviderError,
    Timeout,
    Store(RoomStoreError),
}

/// Pick the model alias this summary runs on: a dedicated `summarize` role, else
/// the generic cheap `fast` role, else the globally bound model.
///
/// Falling back to the global model is what makes the feature work with zero
/// config. `complete_once` requires an explicit alias, so there is no "no alias"
/// path to represent — an unconfigured daemon summarizes on whatever it is
/// already running, which is honest if not cheap. Blank role values are skipped
/// so an empty `[roles]` entry cannot resolve to an unusable model spec.
pub(super) fn resolve_summary_alias(roles: &HashMap<String, String>, global_model: &str) -> String {
    for role in ["summarize", "fast"] {
        if let Some(alias) = roles.get(role).map(|a| a.trim()).filter(|a| !a.is_empty()) {
            return alias.to_string();
        }
    }
    global_model.to_string()
}

/// Derive the `after_seq` cursor that makes the existing ascending, `LIMIT`ed
/// transcript page return the room's NEWEST rows.
///
/// This exists because `load_transcript_page` is `WHERE seq > ?2 ORDER BY seq
/// LIMIT ?3`: reading with the default `None` cursor returns the room's OLDEST
/// rows. Summarizing a thousand-message room would then confidently describe its
/// first two hundred messages and label the result "the room summary" — a
/// false-success of exactly the kind this codebase exists to remove. No new
/// store query is needed: `room_latest_durable_seq` already reports the highest
/// committed `seq`.
///
/// The off-by-one is real and load-bearing. `seq` starts at 0 and `after_seq` is
/// EXCLUSIVE, so a room whose latest seq is 5 holds SIX rows. A naive
/// `latest.saturating_sub(limit)` on a short room yields a cursor that silently
/// drops message 0.
fn tail_cursor(
    latest: Option<u64>,
    effective_limit: usize,
    explicit_after: Option<u64>,
) -> Option<u64> {
    // An explicit window is the caller's business; never second-guess it.
    if explicit_after.is_some() {
        return explicit_after;
    }
    let latest = latest?;
    let limit = effective_limit as u64;
    // `latest + 1` is the row count. Skip rows only when the room is longer than
    // the window; otherwise read from the very beginning (`None`).
    (latest.saturating_add(1) > limit).then(|| latest - limit)
}

/// The summarizer's instruction. It writes for someone who was absent, not for
/// the people who were there, and it never invents what was not said.
fn summary_system_prompt() -> &'static str {
    "You are summarizing a collaboration room's transcript for someone who was \
     not present. Write a compact summary in plain prose: what the room is \
     working on, the decisions it reached, the open questions, and anything \
     someone is expected to do next. Attribute claims to the participant who \
     made them where it matters. Do not invent anything that is not in the \
     transcript, do not narrate that you are summarizing, and do not address \
     the room."
}

/// Render the transcript window as the model's user turn. Oldest → newest, one
/// line per message, in the same `[#seq] author: body` shape `build_room_prompt`
/// hands a convened agent. The two model-facing renderers differ in their
/// framing and in nothing about how a row is rendered.
///
/// Bodies go through `room_history_text`, the SAME projection the four human
/// reads, the agent history page, and `build_room_prompt` apply, so a
/// `room.agent.*` audit row arrives as its fixed label rather than as the
/// principal, decision, and session metadata it carries. That matters more here than on a read: the audit
/// interpolates a free-form `owner_member_id` nothing on the write path
/// shape-checks, and the summary this prompt produces is itself an artifact
/// ocean-surface markdown-renders — an unprojected body would have a laundered
/// route straight back out.
///
/// The AUTHOR label is deliberately left raw. It is the same unbounded
/// caller-supplied identity, but it reaches every human read raw as well, and
/// bounding it belongs where the id is minted rather than in one of four
/// renderers — `os-owner-member-id-is-an-identity-with-no-shape-and-no-bound`
/// owns that. Quoting it here alone would only move the gap.
fn summary_user_prompt(room: &RoomKey, room_name: &str, msgs: &[RoomMessage]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Room \"{room_name}\" (key `{}`). The transcript window below is oldest \
         first.\n\n--- room transcript ---\n",
        room.as_str(),
    ));
    for m in msgs {
        out.push_str(&format!(
            "[#{seq}] {author}: {body}\n",
            seq = m.seq,
            author = m.author_id,
            body = room_history_text(m.body.clone()),
        ));
    }
    out.push_str("--- end transcript ---\n\nYour summary:");
    out
}

/// A model that returned only whitespace said nothing. Writing that into the
/// room as "the summary" would be worse than writing nothing at all.
fn clean_summary_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Build the artifact body: a provenance header, then the model's prose.
///
/// The header exists because the transcript line the store writes says
/// "alice updated 'Room summary' (v3)" — alice asked, but a model wrote the
/// words, and over which messages is not otherwise recoverable. `has_more` says
/// out loud that the window did not reach the start of the room, so a partial
/// summary is honest rather than silently passed off as complete.
fn summary_body(
    model_id: &str,
    text: &str,
    from_seq: u64,
    to_seq: u64,
    count: usize,
    has_more: bool,
    now: DateTime<Utc>,
) -> String {
    let coverage = if has_more {
        " Earlier messages exist before this window; this summary is partial."
    } else {
        ""
    };
    format!(
        "_Generated by `{model_id}` at {ts} from {count} message(s), #{from_seq}–#{to_seq}.{coverage}_\n\n{text}\n",
        ts = now.to_rfc3339(),
    )
}

/// Create the well-known summary artifact, or amend it in place under
/// compare-and-swap.
///
/// The read of the current version and the write that consumes it happen in one
/// call so the daemon-wide store mutex spans both — see `summarize_room`'s phase
/// 3 for why that removes the need for a CAS retry loop here.
fn upsert_summary_artifact(
    store: &mut SqliteRoomStore,
    key: &RoomKey,
    author: &str,
    body: &str,
    now: DateTime<Utc>,
) -> Result<(SummaryWrite, Option<RoomMessage>), RoomStoreError> {
    match store.artifact(key, ROOM_SUMMARY_ARTIFACT_ID)? {
        None => {
            let (artifact, message) = store.create_artifact(
                key,
                ROOM_SUMMARY_ARTIFACT_ID,
                RoomArtifactKind::Note,
                ROOM_SUMMARY_TITLE,
                body,
                author,
                now,
            )?;
            Ok((SummaryWrite::Created(artifact), Some(message)))
        }
        Some(existing) => {
            // Title and state are deliberately left alone: a room that renamed
            // its summary or marked it done keeps that, and only the body moves.
            match store.amend_artifact(
                key,
                ROOM_SUMMARY_ARTIFACT_ID,
                existing.version,
                None,
                Some(body),
                None,
                author,
                now,
            ) {
                Ok((artifact, message)) => Ok((SummaryWrite::Amended(artifact), Some(message))),
                // The model reproduced the identical body. The store refuses a
                // no-op amend on purpose (a version bump would write a transcript
                // line claiming a change that never happened); that refusal is
                // the correct answer here, not an error to surface.
                Err(RoomStoreError::ArtifactUnchanged { .. }) => {
                    Ok((SummaryWrite::Unchanged(existing), None))
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// What phase 1 resolved before any provider work happens.
enum Prepared {
    Ready {
        page: ocean_store::TranscriptPage,
        room_name: String,
    },
    ForgedAuthor,
}

/// Run one summarize pass: read a bounded tail, one model turn, one artifact
/// write.
///
/// STRICTLY THREE PHASES, and the split is a hard requirement rather than a
/// style preference (`crates/ocean-daemon/AGENTS.md`): the room store lives
/// behind a daemon-wide std `Mutex`, so a single closure spanning the provider
/// call would hold it for the whole model latency and stall every other room
/// route in the daemon.
///
///   1. one synchronous store read (preconditions + tail page + room name),
///      guard dropped before returning;
///   2. the `.await` on the provider, holding nothing;
///   3. one synchronous store write, guard dropped before returning.
pub(super) async fn summarize_room<F, Fut, E>(
    rooms: &RoomStoreHandle,
    input: SummarizeInput,
    complete: F,
) -> SummarizeOutcome
where
    F: FnOnce(String, String, String) -> Fut,
    Fut: Future<Output = Result<(String, String), E>>,
{
    let SummarizeInput {
        key,
        requested_by,
        limit,
        after_seq,
        alias,
        timeout,
    } = input;

    // ── Phase 1: read ────────────────────────────────────────────────────────
    let prepared = with_rooms_handle(rooms, |store| {
        // An OPEN room is a precondition, asserted before anything else.
        // `create_artifact`/`amend_artifact` both begin with `room_is_open` →
        // `UnknownRoom`, so a soft-closed room can never gain this artifact. The
        // transcript read would happily succeed against the frozen audit view,
        // which would mean paying for a model turn and then 404ing on the write.
        // Refusing here gives the caller the identical 404 for free.
        let Some(record) = store.get(&key)? else {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        };
        // Same rule and same rejection as `room_create_artifact`: `requested_by`
        // is caller-supplied, and Agent|System are daemon-only author kinds. It
        // is checked BEFORE the provider call so a forged request never costs a
        // model turn.
        let claimed_kind = record
            .room
            .participants
            .iter()
            .find(|p| p.id == requested_by)
            .map(|p| p.kind);
        if matches!(
            claimed_kind,
            Some(RoomParticipantKind::Agent) | Some(RoomParticipantKind::System)
        ) {
            return Ok(Prepared::ForgedAuthor);
        }
        let effective_limit = ocean_store::clamp_transcript_limit(limit);
        let latest = store.room_latest_durable_seq(&key)?;
        let cursor = tail_cursor(latest, effective_limit, after_seq);
        // Paging stays in its owning module so there is exactly one
        // implementation. Its soft-closed audit fallback is unreachable from
        // here by construction — the open-room precondition above already ran.
        let page = read_transcript_page(store, &key, cursor, Some(effective_limit))?;
        Ok(Prepared::Ready {
            page,
            room_name: record.room.name,
        })
    });
    let (page, room_name) = match prepared {
        Ok(Prepared::Ready { page, room_name }) => (page, room_name),
        Ok(Prepared::ForgedAuthor) => return SummarizeOutcome::ForgedAuthor,
        Err(e) => return SummarizeOutcome::Store(e),
    };
    // An empty room has nothing to summarize, and writing "there was no
    // conversation" into an artifact is noise. Nothing is written.
    let (Some(first), Some(last)) = (page.messages.first(), page.messages.last()) else {
        return SummarizeOutcome::NoMessages;
    };
    let (from_seq, to_seq) = (first.seq, last.seq);
    let messages_summarized = page.messages.len();
    let has_more = page.has_more;

    // ── Phase 2: the single model turn, holding no lock ──────────────────────
    let user_prompt = summary_user_prompt(&key, &room_name, &page.messages);
    let completion = complete(alias, summary_system_prompt().to_string(), user_prompt);
    let (raw, model) = match tokio::time::timeout(timeout, completion).await {
        Err(_) => {
            tracing::warn!(room = %key, "room summarize: model call timed out");
            return SummarizeOutcome::Timeout;
        }
        // Provider errors surface the provider's own message, which can embed
        // response fragments. Record the outcome and the room only — never the
        // error body, the transcript, or the summary text.
        Ok(Err(_)) => {
            tracing::warn!(room = %key, "room summarize: model call failed");
            return SummarizeOutcome::ProviderError;
        }
        Ok(Ok(pair)) => pair,
    };
    let Some(text) = clean_summary_text(&raw) else {
        tracing::info!(room = %key, "room summarize: model returned nothing; not writing");
        return SummarizeOutcome::EmptySummary;
    };

    // ── Phase 3: write ───────────────────────────────────────────────────────
    let now = Utc::now();
    let body = summary_body(
        &model,
        &text,
        from_seq,
        to_seq,
        messages_summarized,
        has_more,
        now,
    );
    // The current-version read and the compare-and-swap write that consumes it
    // are in ONE closure, so the daemon-wide store mutex serializes concurrent
    // summarize calls on the same room. That is what makes a CAS retry loop
    // unnecessary: no other summarize pass can slip a version bump between the
    // read and the write.
    let written = with_rooms_handle(rooms, |store| {
        upsert_summary_artifact(store, &key, &requested_by, &body, now)
    });
    match written {
        Ok((SummaryWrite::Unchanged(artifact), _)) => {
            tracing::info!(room = %key, "room summarize: summary unchanged");
            SummarizeOutcome::Unchanged { artifact }
        }
        Ok((write, message)) => {
            let (artifact, created) = match write {
                SummaryWrite::Created(a) => (a, true),
                SummaryWrite::Amended(a) => (a, false),
                SummaryWrite::Unchanged(_) => unreachable!("handled above"),
            };
            let message = message.expect("a create/amend always commits its System line");
            tracing::info!(
                room = %key,
                created,
                version = artifact.version,
                messages_summarized,
                "room summarize: summary written"
            );
            SummarizeOutcome::Wrote {
                artifact,
                created,
                model,
                messages_summarized,
                from_seq,
                to_seq,
                has_more,
                message: Box::new(message),
            }
        }
        Err(e) => SummarizeOutcome::Store(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ocean_core::{RoomMessageKind, RoomParticipant};
    use ocean_store::RoomStore as _;

    use super::*;

    fn roles(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn participant(id: &str, kind: RoomParticipantKind) -> RoomParticipant {
        RoomParticipant {
            id: id.to_string(),
            kind,
            display_name: id.to_string(),
        }
    }

    /// A room with `alice` on the roster and `count` chat messages at seq 0..n.
    /// Returns the shared handle the production path takes.
    fn room_with_messages(key: &RoomKey, count: usize) -> RoomStoreHandle {
        let mut store = SqliteRoomStore::open_in_memory().expect("in-mem store");
        store
            .create(key.clone(), "Map fix", None, Utc::now())
            .expect("create room");
        store
            .add_participant(
                key,
                participant("alice", RoomParticipantKind::Human),
                Utc::now(),
            )
            .expect("join alice");
        for i in 0..count {
            store
                .append_message(
                    key,
                    "alice",
                    RoomParticipantKind::Human,
                    RoomMessageKind::Message,
                    &format!("message {i}"),
                    Utc::now(),
                )
                .expect("append");
        }
        Arc::new(Mutex::new(store))
    }

    fn input(key: &RoomKey, limit: Option<usize>) -> SummarizeInput {
        SummarizeInput {
            key: key.clone(),
            requested_by: "alice".into(),
            limit,
            after_seq: None,
            alias: "test-alias".into(),
            timeout: Duration::from_secs(5),
        }
    }

    /// A provider closure that must never be reached. It flips a flag rather
    /// than panicking so the future keeps a nameable output type; the caller
    /// asserts the flag stayed false, which is the actual claim under test —
    /// this request never cost a model turn.
    fn provider_must_not_run(
        invoked: Arc<std::sync::atomic::AtomicBool>,
    ) -> impl FnOnce(String, String, String) -> std::future::Ready<Result<(String, String), &'static str>>
    {
        move |_, _, _| {
            invoked.store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(("unreachable".into(), "unreachable".into())))
        }
    }

    #[test]
    fn summary_alias_prefers_summarize_then_fast_then_the_bound_model() {
        assert_eq!(
            resolve_summary_alias(&roles(&[("summarize", "haiku"), ("fast", "mini")]), "opus"),
            "haiku"
        );
        assert_eq!(
            resolve_summary_alias(&roles(&[("fast", "mini"), ("advisor", "sonnet")]), "opus"),
            "mini"
        );
        // A blank role value is not a model spec; skip it rather than handing
        // `complete_once` something it cannot resolve.
        assert_eq!(
            resolve_summary_alias(&roles(&[("summarize", "   "), ("fast", "mini")]), "opus"),
            "mini"
        );
        assert_eq!(resolve_summary_alias(&roles(&[]), "opus"), "opus");
    }

    #[test]
    fn tail_cursor_reads_the_newest_rows_without_dropping_message_zero() {
        // Long room: 1000 rows (seq 0..=999), window of 200 → the last 200 rows
        // are seq 800..=999, which is `after_seq = 799`.
        assert_eq!(tail_cursor(Some(999), 200, None), Some(799));
        // Short room: 6 rows (seq 0..=5) and a window of 200 — read from the
        // very beginning. `latest.saturating_sub(limit)` would give 0 here and
        // silently drop message 0, since `after_seq` is EXCLUSIVE.
        assert_eq!(tail_cursor(Some(5), 200, None), None);
        // Exactly full: 200 rows (seq 0..=199) in a 200 window still starts at
        // the beginning, again because seq 0 must survive.
        assert_eq!(tail_cursor(Some(199), 200, None), None);
        // One row past full.
        assert_eq!(tail_cursor(Some(200), 200, None), Some(0));
        // An explicit window always wins, including one that reads nothing.
        assert_eq!(tail_cursor(Some(999), 200, Some(12)), Some(12));
        assert_eq!(tail_cursor(None, 200, Some(12)), Some(12));
        // No messages at all: no cursor.
        assert_eq!(tail_cursor(None, 200, None), None);
    }

    #[test]
    fn user_prompt_names_the_room_and_renders_oldest_to_newest() {
        let key = RoomKey::new("map-fix");
        let msgs = vec![
            RoomMessage {
                seq: 7,
                author_id: "alice".into(),
                author_kind: RoomParticipantKind::Human,
                kind: RoomMessageKind::Message,
                body: "we should revert".into(),
                created_at: Utc::now(),
                federated: None,
                thread_parent_seq: None,
                session_id: None,
                attachment_id: None,
            },
            RoomMessage {
                seq: 8,
                author_id: "bob".into(),
                author_kind: RoomParticipantKind::Human,
                kind: RoomMessageKind::Message,
                body: "agreed".into(),
                created_at: Utc::now(),
                federated: None,
                thread_parent_seq: None,
                session_id: None,
                attachment_id: None,
            },
        ];
        let prompt = summary_user_prompt(&key, "Map fix", &msgs);
        assert!(prompt.contains("Map fix"));
        assert!(prompt.contains("map-fix"));
        let first = prompt.find("[#7] alice: we should revert").expect("row 7");
        let second = prompt.find("[#8] bob: agreed").expect("row 8");
        assert!(first < second, "transcript must render oldest first");
    }

    #[test]
    fn whitespace_only_summaries_are_not_summaries() {
        assert_eq!(clean_summary_text("   \n\t "), None);
        assert_eq!(clean_summary_text(""), None);
        assert_eq!(
            clean_summary_text("  the room reverted the map change.  "),
            Some("the room reverted the map change.".to_string())
        );
    }

    #[test]
    fn summary_body_records_provenance_and_admits_a_partial_window() {
        let now = Utc::now();
        let complete = summary_body("haiku-x", "they reverted it", 800, 999, 200, false, now);
        assert!(complete.contains("haiku-x"));
        assert!(complete.contains("#800–#999"));
        assert!(complete.contains("200 message(s)"));
        assert!(complete.contains("they reverted it"));
        assert!(!complete.contains("partial"));
        let partial = summary_body("haiku-x", "they reverted it", 800, 999, 200, true, now);
        assert!(
            partial.contains("partial"),
            "a window that did not reach the start of the room must say so"
        );
    }

    #[test]
    fn first_summary_creates_the_well_known_note_and_explains_itself() {
        let key = RoomKey::new("upsert-create");
        let rooms = room_with_messages(&key, 1);
        let (write, message) = with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", "first body", Utc::now())
        })
        .expect("create");

        let SummaryWrite::Created(artifact) = write else {
            panic!("first write must create");
        };
        assert_eq!(artifact.id, ROOM_SUMMARY_ARTIFACT_ID);
        assert_eq!(artifact.title, ROOM_SUMMARY_TITLE);
        assert_eq!(artifact.kind, RoomArtifactKind::Note);
        assert_eq!(artifact.version, 1);
        assert_eq!(artifact.body, "first body");
        let message = message.expect("a create commits its System line");
        assert_eq!(message.kind, RoomMessageKind::System);
    }

    #[test]
    fn repeated_summaries_amend_one_artifact_rather_than_accumulating() {
        let key = RoomKey::new("upsert-amend");
        let rooms = room_with_messages(&key, 1);
        with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", "first body", Utc::now())
        })
        .expect("create");
        let (write, message) = with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", "second body", Utc::now())
        })
        .expect("amend");

        let SummaryWrite::Amended(artifact) = write else {
            panic!("second write must amend");
        };
        assert_eq!(artifact.version, 2);
        assert_eq!(artifact.body, "second body");
        assert!(message.is_some());
        // The whole point of the well-known id: the room owns ONE summary.
        let artifacts = with_rooms_handle(&rooms, |store| store.artifacts(&key)).expect("list");
        assert_eq!(
            artifacts.len(),
            1,
            "summarizing twice must not add a second artifact"
        );
    }

    #[test]
    fn an_identical_summary_moves_nothing_and_writes_no_transcript_line() {
        let key = RoomKey::new("upsert-unchanged");
        let rooms = room_with_messages(&key, 1);
        with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", "same body", Utc::now())
        })
        .expect("create");
        let before = with_rooms_handle(&rooms, |store| store.transcript(&key, None)).expect("read");

        let (write, message) = with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", "same body", Utc::now())
        })
        .expect("unchanged is not an error here");

        let SummaryWrite::Unchanged(artifact) = write else {
            panic!("an identical body must not bump the version");
        };
        assert_eq!(artifact.version, 1);
        assert!(message.is_none());
        let after = with_rooms_handle(&rooms, |store| store.transcript(&key, None)).expect("read");
        assert_eq!(
            before.len(),
            after.len(),
            "a no-op summary must not tell the room something changed"
        );
    }

    #[test]
    fn a_non_roster_author_cannot_own_the_summary() {
        let key = RoomKey::new("upsert-stranger");
        let rooms = room_with_messages(&key, 1);
        let err = with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "mallory", "body", Utc::now())
        })
        .expect_err("a stranger is not in the room");
        assert!(matches!(
            err,
            RoomStoreError::ArtifactAuthorNotInRoster { .. }
        ));
    }

    #[tokio::test]
    async fn a_room_with_no_rows_never_reaches_the_provider() {
        // A freshly created room nobody has joined: the roster is empty and the
        // transcript has not even a join marker. There is nothing to summarize.
        let key = RoomKey::new("bare");
        let mut store = SqliteRoomStore::open_in_memory().expect("in-mem store");
        store
            .create(key.clone(), "Bare", None, Utc::now())
            .expect("create");
        let rooms: RoomStoreHandle = Arc::new(Mutex::new(store));

        let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = summarize_room(
            &rooms,
            input(&key, None),
            provider_must_not_run(invoked.clone()),
        )
        .await;
        assert!(matches!(outcome, SummarizeOutcome::NoMessages));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "a room with no messages must never cost a model turn"
        );
        let artifacts = with_rooms_handle(&rooms, |store| store.artifacts(&key)).expect("list");
        assert!(artifacts.is_empty(), "nothing is written for an empty room");
    }

    #[tokio::test]
    async fn a_provider_failure_writes_nothing_and_never_formats_the_error() {
        struct SecretProviderError {
            _secret: &'static str,
        }

        let key = RoomKey::new("provider-error");
        let rooms = room_with_messages(&key, 3);
        let outcome = summarize_room(&rooms, input(&key, None), |_, _, _| async {
            Err::<(String, String), _>(SecretProviderError {
                _secret: "must-never-be-formatted-or-logged",
            })
        })
        .await;
        assert!(matches!(outcome, SummarizeOutcome::ProviderError));
        let artifacts = with_rooms_handle(&rooms, |store| store.artifacts(&key)).expect("list");
        assert!(
            artifacts.is_empty(),
            "a failed model call must write nothing"
        );
    }

    #[tokio::test]
    async fn a_whitespace_only_model_reply_writes_nothing() {
        let key = RoomKey::new("empty-summary");
        let rooms = room_with_messages(&key, 3);
        let outcome = summarize_room(&rooms, input(&key, None), |_, _, _| async {
            Ok::<_, &'static str>(("  \n  ".into(), "model-x".into()))
        })
        .await;
        assert!(matches!(outcome, SummarizeOutcome::EmptySummary));
        let artifacts = with_rooms_handle(&rooms, |store| store.artifacts(&key)).expect("list");
        assert!(artifacts.is_empty());
    }

    #[tokio::test]
    async fn a_long_room_summarizes_its_newest_window_not_its_oldest() {
        let key = RoomKey::new("long");
        // 1 join marker + 30 chat rows ⇒ seq 0..=30.
        let rooms = room_with_messages(&key, 30);
        let outcome = summarize_room(&rooms, input(&key, Some(10)), |_, _, user| async move {
            // The prompt must carry the LAST ten rows, not the first ten.
            assert!(
                user.contains("message 29"),
                "newest row must be in the window"
            );
            assert!(
                !user.contains("message 0\n"),
                "the oldest rows must be outside a tail window"
            );
            Ok::<_, &'static str>(("tail summary".into(), "model-x".into()))
        })
        .await;

        let SummarizeOutcome::Wrote {
            artifact,
            created,
            model,
            messages_summarized,
            from_seq,
            to_seq,
            has_more,
            ..
        } = outcome
        else {
            panic!("expected a written summary");
        };
        assert!(created);
        assert_eq!(model, "model-x");
        assert_eq!(messages_summarized, 10);
        assert_eq!((from_seq, to_seq), (21, 30));
        assert!(!has_more, "a tail read reaches the end of the room");
        assert_eq!(artifact.version, 1);
        assert!(artifact.body.contains("tail summary"));
        assert!(artifact.body.contains("#21–#30"));
    }

    /// The fifth read of a `room.agent.*` audit row, and one of the two that
    /// laundered it — `build_room_prompt` is the other, pinned by
    /// `a_convened_agents_transcript_tail_projects_an_audit_row`.
    /// `owner_member_id` is free-form and shape-checked nowhere, and the
    /// artifact this pass writes is markdown-rendered by ocean-surface, so a raw
    /// body was a route from an operator-supplied string through a model turn
    /// and into a room-attributed link.
    #[tokio::test]
    async fn a_bootstrap_audit_row_reaches_the_model_as_a_label_not_as_its_ids() {
        const POISON_OWNER: &str = "[click here](https://evil.co)";
        const PACKAGE: &str = "pkg-interpolated-only-into-the-audit";
        const OPERATOR: &str = "operator:only-in-the-audit";

        let key = RoomKey::new("audit-into-the-prompt");
        let rooms = room_with_messages(&key, 2);
        // A real bootstrap, not a hand-written body: the row under test has to be
        // the one the store actually mints, or the projection is asserted against
        // a shape nothing writes.
        let bootstrap = with_rooms_handle(&rooms, |store| {
            store.add_participant(
                &key,
                participant(POISON_OWNER, RoomParticipantKind::Human),
                Utc::now(),
            )?;
            store.bootstrap_local_room_agent(
                &key,
                POISON_OWNER,
                participant("builder", RoomParticipantKind::Agent),
                PACKAGE,
                OPERATOR,
                Utc::now(),
            )
        })
        .expect("bootstrap");
        let audit_seq = bootstrap
            .audit_message
            .expect("a first bootstrap mints the audit row")
            .seq;

        let seen = Arc::new(Mutex::new(String::new()));
        let captured = seen.clone();
        let outcome = summarize_room(&rooms, input(&key, None), move |_, _, user| {
            *captured.lock().expect("prompt") = user;
            std::future::ready(Ok::<_, &'static str>((
                "they bootstrapped an agent".into(),
                "model-x".into(),
            )))
        })
        .await;
        assert!(matches!(outcome, SummarizeOutcome::Wrote { .. }));

        let prompt = seen.lock().expect("prompt").clone();
        assert!(
            prompt.contains(&format!(
                "[#{audit_seq}] system: [room agent bootstrap audit]\n"
            )),
            "the audit row must reach the model as its fixed label: {prompt}"
        );
        // Every string only the audit body interpolates. The join markers carry
        // the owner id too, so asserting on those would pass for the wrong reason.
        for leaked in [PACKAGE, OPERATOR, "room.agent.bootstrap", "owner_member_id"] {
            assert!(
                !prompt.contains(leaked),
                "`{leaked}` rode into the model turn: {prompt}"
            );
        }
        assert!(
            prompt.contains("message 1"),
            "an ordinary body is not projected"
        );

        // The author label is still raw, and this pins that rather than leaving
        // it to be discovered: it is the same unbounded identity, it reaches
        // every human read raw as well, and it is bounded where the id is minted
        // by `os-owner-member-id-is-an-identity-with-no-shape-and-no-bound`.
        assert!(prompt.contains(&format!("{POISON_OWNER}:")));

        // The ledger is untouched: this projects the read, never the record.
        let stored = with_rooms_handle(&rooms, |store| store.transcript(&key, None)).expect("read");
        assert!(
            stored
                .iter()
                .any(|m| m.body.contains(POISON_OWNER) && m.body.contains(PACKAGE)),
            "the audit row must still hold verbatim what was attempted"
        );
    }

    #[tokio::test]
    async fn an_agent_may_not_be_named_as_the_requester() {
        let key = RoomKey::new("forged");
        let rooms = room_with_messages(&key, 2);
        with_rooms_handle(&rooms, |store| {
            store.add_participant(
                &key,
                participant("scribe", RoomParticipantKind::Agent),
                Utc::now(),
            )
        })
        .expect("register agent");

        let mut forged = input(&key, None);
        forged.requested_by = "scribe".into();
        let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = summarize_room(&rooms, forged, provider_must_not_run(invoked.clone())).await;
        assert!(matches!(outcome, SummarizeOutcome::ForgedAuthor));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "a forged author is refused before the provider is called"
        );
    }

    #[tokio::test]
    async fn an_unknown_or_closed_room_is_refused_before_any_model_work() {
        let key = RoomKey::new("gone");
        let rooms = room_with_messages(&key, 2);
        let missing = RoomKey::new("never-existed");
        let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = summarize_room(
            &rooms,
            input(&missing, None),
            provider_must_not_run(invoked.clone()),
        )
        .await;
        assert!(matches!(
            outcome,
            SummarizeOutcome::Store(RoomStoreError::UnknownRoom(_))
        ));

        // A soft-closed room reads fine but can never gain an artifact
        // (`create_artifact`/`amend_artifact` require `room_is_open`), so the
        // 404 is raised up front instead of after paying for a model turn.
        with_rooms_handle(&rooms, |store| store.close(&key)).expect("close");
        let outcome = summarize_room(
            &rooms,
            input(&key, None),
            provider_must_not_run(invoked.clone()),
        )
        .await;
        assert!(matches!(
            outcome,
            SummarizeOutcome::Store(RoomStoreError::UnknownRoom(_))
        ));
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "neither an unknown nor a closed room may cost a model turn"
        );
    }

    #[tokio::test]
    async fn a_stalled_provider_is_bounded_by_the_timeout() {
        let key = RoomKey::new("stalled");
        let rooms = room_with_messages(&key, 2);
        let mut stalling = input(&key, None);
        stalling.timeout = Duration::from_millis(10);
        let outcome = summarize_room(&rooms, stalling, |_, _, _| {
            std::future::pending::<Result<(String, String), &'static str>>()
        })
        .await;
        assert!(matches!(outcome, SummarizeOutcome::Timeout));
        let artifacts = with_rooms_handle(&rooms, |store| store.artifacts(&key)).expect("list");
        assert!(artifacts.is_empty());
    }

    #[tokio::test]
    async fn re_summarizing_an_unchanged_room_reports_unchanged_not_an_error() {
        let key = RoomKey::new("stable");
        let rooms = room_with_messages(&key, 2);
        // Pin `now` out of the body by writing the same artifact body twice: the
        // provenance header carries a timestamp, so drive the second pass through
        // `upsert_summary_artifact` with the body the first pass produced.
        let first = summarize_room(&rooms, input(&key, None), |_, _, _| async {
            Ok::<_, &'static str>(("stable summary".into(), "model-x".into()))
        })
        .await;
        let SummarizeOutcome::Wrote { artifact, .. } = first else {
            panic!("expected a written summary");
        };
        let (write, message) = with_rooms_handle(&rooms, |store| {
            upsert_summary_artifact(store, &key, "alice", &artifact.body, Utc::now())
        })
        .expect("unchanged is a clean answer");
        assert!(matches!(write, SummaryWrite::Unchanged(_)));
        assert!(message.is_none());
    }
}
