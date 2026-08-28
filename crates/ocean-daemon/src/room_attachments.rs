//! Room attachments: the doc, the spec, the screenshot a room needs everybody
//! to be looking at.
//!
//! `ocean-store` indexes attachments (`room_attachments`, one row per file);
//! this module owns everything about the BYTES — where they live, how big they
//! are allowed to be, what id they are filed under, and how they are served
//! back. The split is deliberate and matches the rest of the daemon: SQL lives
//! in `ocean-store`, HTTP and filesystem live here.
//!
//! Four handlers, one durable directory tree:
//!
//! ```text
//! POST   /v1/rooms/persistent/{key}/attachments        upload
//! GET    /v1/rooms/persistent/{key}/attachments        list (metadata only)
//! GET    /v1/rooms/persistent/{key}/attachments/{id}   download
//! DELETE /v1/rooms/persistent/{key}/attachments/{id}   remove row + bytes
//! ```
//!
//! Three rules run through all of it, and none of them is negotiable:
//!
//! 1. **No caller-supplied string ever becomes a path component.** Not the
//!    filename, not the attachment id, and — the one the brief did not name —
//!    not the room key. `RoomKey::new` performs zero validation
//!    (`ocean-core/src/lib.rs`: it is literally `Self(value.into())`), keys in
//!    the wild already look like `call:xyz`, and nothing stops `../../..`, a
//!    4 KB key, or two keys that APFS case-folds onto one directory. The room
//!    directory is therefore `sha256(key)` in hex — always derived at use, never
//!    stored — which removes traversal, length, and case-folding collisions in
//!    one move. The attachment id is server-minted AND re-validated as
//!    `[0-9a-f]{32}` before every filesystem call, because "it was safe when we
//!    minted it" is not a property the URL parser preserves.
//! 2. **The DECLARED content type is recorded and never acted on.** The daemon
//!    serves browser origins (loopback, `chrome-extension://`,
//!    `tauri://localhost`), so echoing an uploader-declared `text/html` back on
//!    a download is stored XSS against ocean-surface. What a download serves is
//!    therefore either `application/octet-stream` or a type DERIVED FROM THE
//!    BYTES by [`sniff_image_content_type`] — never the uploader's string. The
//!    derivation is a CLOSED allowlist of non-scriptable raster image
//!    signatures (PNG, JPEG, GIF, WebP, and deliberately never SVG), so the
//!    worst a mis-derivation can claim is that some bytes are a PNG, and
//!    `X-Content-Type-Options: nosniff` stays on in EVERY branch precisely so
//!    the browser cannot then re-interpret those bytes as anything else.
//!    `Content-Disposition: attachment` also stays on in every branch: a
//!    browser renders an `<img src>` subresource regardless of it, which is all
//!    an inline-media surface needs, so there is no reason to also surrender
//!    the top-level-navigation defence to buy something already in hand. "Every
//!    branch" includes every FILENAME, which is why [`content_disposition`]
//!    encodes the name rather than formatting it: a filename is UTF-8 and a
//!    header parameter is not, and the naive version dropped the entire header
//!    on the names it could not spell — losing the defence to say nothing about
//!    a name.
//! 3. **Bytes are written and fsynced BEFORE the row commits.** The two writes
//!    cannot be one transaction — one is SQLite, one is the filesystem — so the
//!    order is chosen by which residue is survivable. An orphan blob is
//!    unreferenced garbage the upload path immediately unlinks (and a crash
//!    leaves it for a future GC sweep). An orphan row is a download that 500s
//!    forever.
//!
//! This module still stops at the bytes. Prompt assembly lives in
//! `room_context.rs`, which reads through [`attachment_bytes`] — the ONE
//! in-process surface widened past the private path helpers, so the hashed
//! directory and the id validation keep exactly one implementation. What is
//! deliberately still absent is the Ocean Rooms v2 §7
//! `ContextPolicy`/`ContextMount` model the root `AGENTS.md` forbids
//! implementing from the proposal alone: there is no per-agent selection and no
//! declared mount, only "a room's files are the room's shared context".

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use ocean_core::{RoomKey, RoomParticipantKind};
use ocean_store::RoomStore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::persistent_rooms::{
    invalid_request_response, publish_room_wake, room_store_error_response, with_rooms,
};
use crate::AppState;

/// Hard ceiling on one attachment. A room context file is a spec, a screenshot,
/// or a PDF; 8 MiB covers those with room to spare and keeps a single upload
/// from parking that much of the daemon's heap per concurrent request.
///
/// This is enforced twice, on purpose: `DefaultBodyLimit` at the route refuses
/// anything far over it without buffering, and the handler refuses anything over
/// it with a typed `attachment_too_large` body. See [`BODY_LIMIT_SLACK`].
pub(super) const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// How far above [`MAX_ATTACHMENT_BYTES`] the route's body limit sits.
///
/// axum-core imposes a 2 MiB default body limit, so WITHOUT an explicit
/// `DefaultBodyLimit` layer an 8 MiB cap would be fiction and every upload past
/// 2 MiB would die with an untyped 413 that looks like our bug. With the layer
/// set to cap + slack, a body a little over the cap still reaches the handler
/// and gets the typed JSON rejection that tells the client what the limit is,
/// while a gigabyte body is refused by the layer and never buffered at all.
pub(super) const BODY_LIMIT_SLACK: usize = 4096;

/// Longest declared content type we will record. Long enough for any real MIME
/// type with parameters, short enough that the column is not a free text field.
const MAX_CONTENT_TYPE_LEN: usize = 128;

/// Longest filename we will record. Display only — see [`sanitize_filename`].
const MAX_FILENAME_LEN: usize = 128;

// ---- Path derivation --------------------------------------------------------

/// Root of the attachment blob tree.
///
/// It sits next to the `rooms.db` that indexes it, so `OCEAN_DB_PATH` moves the
/// metadata and the bytes together instead of splitting a room's file across two
/// unrelated locations. Resolved ONCE at startup and carried on
/// [`AppState::room_attachments_root`] rather than re-read from the environment
/// per request — the same TASK-58 rule the runtime config dir already follows,
/// so parallel tests can inject a tempdir instead of racing on process env.
pub(super) fn room_attachments_root() -> std::path::PathBuf {
    let db = crate::persistent_rooms::room_db_path();
    match db.parent() {
        Some(parent) => parent.join("room-attachments"),
        // `room_db_path()` always has a parent in practice; a bare relative file
        // name is the only way here, and a relative sibling directory is the
        // right answer for it.
        None => std::path::PathBuf::from("room-attachments"),
    }
}

/// Where one room's blobs live: `<root>/<hex sha256 of the room key>`.
///
/// The hash is the whole traversal defence. A room key is an unvalidated
/// free-form string, so joining it raw would let `../../..` (or a key long
/// enough to blow the filesystem's name limit, or two keys APFS folds together)
/// escape or collide. Hashing is one-way and fixed-width, so the derived
/// directory is always exactly 64 hex characters and always inside the root.
/// Never stored: it is recomputed on every use, so there is no persisted path to
/// go stale or be tampered with.
fn room_dir(root: &std::path::Path, key: &RoomKey) -> std::path::PathBuf {
    let mut digest = Sha256::new();
    digest.update(key.as_str().as_bytes());
    root.join(format!("{:x}", digest.finalize()))
}

/// Is this a well-formed attachment id? Exactly 32 lowercase hex characters.
///
/// Ids are server-minted, so in practice this cannot fail — which is exactly why
/// it is re-checked before every filesystem call. The value arriving at
/// [`room_download_attachment`] came off the URL, and the only thing standing
/// between "we minted safe ids" and "someone typed `../../rooms.db`" is a
/// validator that runs on the value actually in hand.
fn is_attachment_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The blob path for one attachment, or `None` if the id is not well-formed.
///
/// Returning `Option` rather than building a path and hoping keeps the check on
/// the only route to the filesystem: there is no way to obtain a path for a
/// malformed id.
fn blob_path(root: &std::path::Path, key: &RoomKey, id: &str) -> Option<std::path::PathBuf> {
    is_attachment_id(id).then(|| room_dir(root, key).join(id))
}

/// A fresh attachment id. v4 UUID, hyphens stripped: 32 lowercase hex chars, so
/// it satisfies [`is_attachment_id`] by construction.
fn mint_attachment_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Reduce an uploader-supplied filename to something safe to store and to echo
/// in a `Content-Disposition` header.
///
/// This is cheap on purpose. The filename is DISPLAY ONLY — it never becomes a
/// path component (the blob is filed under the attachment id), so this does not
/// need to be a hardened path sanitizer, and nobody should later "improve" it
/// into one. What it does need to guarantee is that the value cannot inject a
/// header or a transcript line: control characters are stripped, any directory
/// prefix a browser tacked on is dropped, `.`/`..` are refused outright, and the
/// result is bounded.
fn sanitize_filename(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(MAX_FILENAME_LEN)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned)
}

/// The download's `Content-Disposition`, for any filename a row can hold.
///
/// [`sanitize_filename`] deliberately keeps non-ASCII characters — `café.png`
/// is the name the uploader gave the file and the name the room should show —
/// and formatting one straight into the parameter fails in two directions at
/// once, so this owns both.
///
/// The first is a WRONG NAME. `HeaderValue` accepts `0x80..=0xff` (RFC 9110
/// `obs-text`), so raw UTF-8 does reach the wire — but RFC 6266's `filename` is
/// a bare quoted-string with no charset, and clients guess differently:
/// Firefox and Chrome read those bytes as Latin-1 and save `cafÃ©.png`. So the
/// name is carried twice, the way RFC 6266 §4.3 says to — an ASCII skeleton in
/// `filename=` for clients that only parse that, and an RFC 5987
/// `filename*=UTF-8''…` that says what the bytes actually mean. Skeleton, not
/// transliteration: every non-graphic char becomes `_`, so `café.png` is
/// `caf_.png` and `日本語.png` is `___.png`. There is no folding table here.
///
/// The second is a MISSING HEADER. `from_str` does refuse `0x00..=0x1f` and
/// `0x7f`, and the caller used to drop the whole header when it did, rather
/// than fall back to the id as its comment claimed — an outcome the module
/// header's rule 2 cannot afford, since the derived `Content-Type` rests on
/// `attachment` being unconditional. Every byte this builds is visible ASCII,
/// so the value cannot fail and the header is PRESENT in every branch.
fn content_disposition(filename: &str, id: &str) -> HeaderValue {
    // Space is legal inside a quoted-string and common in real filenames; `"`
    // and `\` are the two that would end or escape it.
    let ascii: String = filename
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c == ' ' || c.is_ascii_graphic() => c,
            _ => '_',
        })
        .collect();
    let ascii = ascii.trim();
    if ascii.is_empty() {
        // Only a row `sanitize_filename` never saw can land here. Fall back to
        // the id, which is `[0-9a-f]{32}` by construction and so can never fail
        // the way the name it replaces did.
        return HeaderValue::from_str(&format!("attachment; filename=\"{id}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    }

    if ascii == filename {
        // Nothing was lost, so `filename*` would only repeat what is already
        // there — and a second parameter is a second thing a client can
        // disagree with us about.
        return HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(filename.len());
    for b in filename.bytes() {
        // RFC 5987 `attr-char`. Everything else — `%` itself included — is
        // escaped, so the value cannot grow a `;` or a `"` of its own.
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[usize::from(b >> 4)] as char);
            encoded.push(HEX[usize::from(b & 0x0f)] as char);
        }
    }

    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    // Unreachable: every byte above is visible ASCII. A bare `attachment` still
    // refuses the top-level render, which is the property rule 2 depends on.
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// Is the DECLARED content type storable? Bounded, non-empty, visible ASCII.
///
/// This validates shape, not meaning: nothing downstream trusts the value, so
/// there is no allowlist of real MIME types to check against. The bound exists
/// so the column cannot become an arbitrary-length client-controlled blob, and
/// the visible-ASCII rule so the recorded string can never carry a newline into
/// a log line or a rendered view.
fn is_storable_content_type(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CONTENT_TYPE_LEN
        && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// The content type DERIVED from an attachment's leading bytes, or `None` when
/// nothing on the allowlist matches.
///
/// This is the counterpart to [`is_storable_content_type`], and the split
/// between them is the whole rule: the DECLARATION is shape-checked, stored, and
/// never served, while this — computed from bytes the row already vouches for —
/// is the only thing that ever reaches a `Content-Type` header.
///
/// The allowlist is CLOSED and holds only raster formats, which have no script
/// surface for a browser to execute. SVG is absent on purpose and must stay
/// absent: it is XML that can carry `<script>`, and it has no magic bytes to
/// recognise it by in the first place, so admitting it would mean trusting
/// either the declaration or a content heuristic — the two things this module
/// refuses to do. Anything unrecognised falls back to `application/octet-stream`
/// at the call site rather than being guessed at.
///
/// Every comparison is a `starts_with` or a bounds-checked window, so a file
/// shorter than a signature matches nothing instead of panicking.
fn sniff_image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // WebP is the one signature that is not a prefix: `RIFF`, then a four-byte
    // chunk length carrying no format information, then `WEBP`. Matching on
    // `RIFF` alone would claim every AVI and WAV file as an image, so the
    // eight-byte gap has to be stepped over rather than ignored.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

// ---- Wire types -------------------------------------------------------------

/// Upload metadata, carried in the QUERY STRING rather than custom headers.
///
/// This is not a style choice. `cors.rs` allows exactly `content-type` and
/// `authorization` on cross-origin requests, so any `X-Ocean-Attachment-*`
/// header would work under curl and fail the browser preflight — breaking
/// ocean-surface uploads in a way no Rust test would catch. The body is
/// therefore raw bytes with its `Content-Type` header ignored entirely, and
/// every piece of metadata travels in the URL.
#[derive(Debug, Deserialize)]
pub(super) struct UploadAttachmentQuery {
    /// What to call the file in the room. Display only.
    filename: String,
    /// The uploader's DECLARED content type. Recorded, never trusted.
    content_type: String,
    /// Participant id of the uploader. Roster-checked inside the store
    /// transaction; caller-asserted, exactly like `author_id` on the artifact
    /// routes.
    uploader_id: String,
}

/// Who is removing an attachment.
///
/// A query parameter rather than a request body because DELETE-with-body is
/// unreliable from browsers and proxies, and both the roster check and the
/// transcript marker need an identity to name.
#[derive(Debug, Deserialize)]
pub(super) struct DeleteAttachmentQuery {
    actor_id: String,
}

// ---- Shared gates -----------------------------------------------------------

/// Refuse a write that claims an Agent's or System's identity.
///
/// Ported from `room_create_artifact`'s Finding-B check, which is a
/// mutation-tested rule: `uploader_id` is caller-supplied and only
/// roster-checked, so without this a hostile local caller could attach a file AS
/// somebody's agent, or as the daemon's own `system` author. An agent's work
/// product comes from the daemon's convene path, never from a client claiming
/// its identity over the wire. Returns the ready-made 403 when the claim is
/// forged, `None` when it is fine (including for an id that is not on the roster
/// at all — that is the store's refusal to make, inside its transaction).
fn forged_author_response(
    state: &AppState,
    key: &RoomKey,
    claimed: &str,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    let claimed_kind = with_rooms(state, |store| store.get(key))
        .ok()
        .and_then(|rec| {
            rec.and_then(|rec| {
                rec.room
                    .participants
                    .iter()
                    .find(|p| p.id == claimed)
                    .map(|p| p.kind)
            })
        });
    matches!(
        claimed_kind,
        Some(RoomParticipantKind::Agent) | Some(RoomParticipantKind::System)
    )
    .then(|| {
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "code": "forged_attachment_author",
                "error": "an agent's attachment is written by the daemon, not by a client claiming its identity",
            })),
        )
    })
}

/// The 400 for an attachment id that is not `[0-9a-f]{32}`.
///
/// Its own code (rather than the generic `invalid_request`) because this is the
/// traversal rejection, and an operator reading a log wants to see that
/// distinctly from an ordinary malformed body.
fn malformed_attachment_id_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "ok": false,
            "code": "malformed_attachment_id",
            "error": "an attachment id is exactly 32 lowercase hex characters",
        })),
    )
}

// ---- Handlers ---------------------------------------------------------------

/// `POST /v1/rooms/persistent/{key}/attachments` — put a file in the room.
///
/// The body is raw bytes (the `voice_stt` precedent), so an 8 MiB file costs
/// 8 MiB rather than base64's 11. Ordering is load-bearing: everything cheap and
/// refusable happens before a single byte is written, the blob is durable before
/// the row commits, and a store failure unlinks what was just written.
pub(super) async fn room_upload_attachment(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<UploadAttachmentQuery>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    if key.as_str().is_empty() {
        return invalid_request_response();
    }
    let Some(filename) = sanitize_filename(&query.filename) else {
        return invalid_request_response();
    };
    let content_type = query.content_type.trim();
    if !is_storable_content_type(content_type) {
        return invalid_request_response();
    }
    let uploader = query.uploader_id.trim();
    if uploader.is_empty() {
        return invalid_request_response();
    }
    // An empty attachment is a client bug every time: there is no room context
    // in zero bytes, and admitting it costs a row, a blob, and a transcript line
    // that all say nothing.
    if body.is_empty() {
        return invalid_request_response();
    }
    if body.len() > MAX_ATTACHMENT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "ok": false,
                "code": "attachment_too_large",
                "error": format!(
                    "attachment is {} bytes; the limit is {MAX_ATTACHMENT_BYTES}",
                    body.len()
                ),
                "max_bytes": MAX_ATTACHMENT_BYTES,
            })),
        );
    }
    // Cheap pre-check so an unknown room is a friendly 404 before we touch the
    // filesystem. It is NOT the authority — the store re-checks room-open and
    // roster inside its own transaction, so a room closing mid-upload still
    // fails correctly and leaves nothing but a blob we unlink below.
    let room_known = with_rooms(&state, |store| store.get(&key))
        .map(|rec| rec.is_some())
        .unwrap_or(false);
    if !room_known {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "code": "unknown_room",
                "error": format!("no open room with key '{key}'"),
            })),
        );
    }
    if let Some(refusal) = forged_author_response(&state, &key, uploader) {
        return refusal;
    }

    let id = mint_attachment_id();
    let sha256 = format!("{:x}", Sha256::digest(&body));
    let byte_len = body.len() as u64;
    let root = state.room_attachments_root.as_path();
    let dir = room_dir(root, &key);
    let Some(path) = blob_path(root, &key, &id) else {
        // Unreachable: `mint_attachment_id` produces 32 hex chars. If it ever
        // stops doing so, refusing is far better than writing to a path we did
        // not validate.
        return internal_error("minted attachment id failed its own validation");
    };
    if let Err(e) = write_blob(&dir, &path, &body) {
        tracing::warn!(room = %key, error = %e, "room attachment blob write failed");
        return internal_error("could not store the attachment bytes");
    }

    let result = with_rooms(&state, |store| {
        store.add_attachment(
            &key,
            &id,
            &filename,
            content_type,
            byte_len,
            &sha256,
            uploader,
            Utc::now(),
        )
    });
    match result {
        Ok((attachment, message)) => {
            // The marker is live on the room's SSE tail, so every client learns
            // the file exists without polling.
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::CREATED,
                Json(json!({ "ok": true, "attachment": attachment })),
            )
        }
        Err(e) => {
            // The row did not commit, so nothing references these bytes. Best
            // effort: a failed unlink leaves unreachable garbage, which is the
            // survivable half of this trade.
            let _ = std::fs::remove_file(&path);
            room_store_error_response(e)
        }
    }
}

/// `GET /v1/rooms/persistent/{key}/attachments` — what is in this room.
///
/// Metadata only; the bytes are one more request away. An unknown room returns
/// an empty list rather than a 404, matching `room_list_artifacts`. That is a
/// wart — the upload route on the same path 404s — but two sibling endpoints
/// disagreeing about a missing room is worse than one consistent wart, and
/// changing the artifact behavior is not this feature's business.
pub(super) async fn room_list_attachments(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    match with_rooms(&state, |store| store.attachments(&key)) {
        Ok(attachments) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "attachments": attachments })),
        ),
        Err(e) => room_store_error_response(e),
    }
}

/// `GET /v1/rooms/persistent/{key}/attachments/{attachment_id}` — the bytes.
///
/// Never the declared type. What goes on the wire is either
/// `application/octet-stream` or an image type [`sniff_image_content_type`]
/// derived from the bytes themselves. See the module header: the daemon answers
/// browser origins, so reflecting an uploader-chosen `text/html` here is stored
/// XSS against ocean-surface.
///
/// `nosniff` and `Content-Disposition: attachment` are unconditional in BOTH
/// branches, and that is what keeps a derived type from being an escalation: a
/// browser told `image/png` with `nosniff` will not render the body as anything
/// but an image, whatever the bytes turn out to be. `attachment` stays because
/// an `<img src>` subresource renders regardless of it — the inline case this
/// derivation exists for is already paid for by the type alone. Unconditional
/// means for every filename too: [`content_disposition`] owns the encoding so
/// that a name a header cannot spell verbatim costs the name's spelling, never
/// the header.
///
/// The row is the authority and the disk is a cache, so the stored bytes are
/// re-checked against the row's length and hash on every read. That is O(n) with
/// n bounded by the 8 MiB cap, and it turns a truncated, swapped, or
/// half-written file into an honest 500 instead of a silently wrong download.
pub(super) async fn room_download_attachment(
    State(state): State<AppState>,
    Path((key, attachment_id)): Path<(String, String)>,
) -> Response {
    let key = RoomKey::new(key.trim());
    let id = attachment_id.trim();
    // Shape first, BEFORE any filesystem access — a malformed id never reaches
    // a path-building call at all.
    let Some(path) = blob_path(state.room_attachments_root.as_path(), &key, id) else {
        return malformed_attachment_id_response().into_response();
    };
    let row = match with_rooms(&state, |store| store.attachment(&key, id)) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "code": "unknown_attachment",
                    "error": format!("room '{key}' has no attachment '{id}'"),
                })),
            )
                .into_response()
        }
        Err(e) => return room_store_error_response(e).into_response(),
    };

    let Some(bytes) = read_verified_blob(&path, &key, &row) else {
        return bytes_missing_response(&key, id);
    };

    // Derived from the bytes just verified against the row — never from
    // `row.content_type`, which is only the uploader's word for it.
    let served_type = sniff_image_content_type(&bytes).unwrap_or("application/octet-stream");

    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(served_type));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Unconditional, and `content_disposition` is what makes it so: a filename
    // is UTF-8 and a header is not, so the encoding has to be its job rather
    // than a `format!` that can quietly fail.
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition(&row.filename, id),
    );
    response
}

/// `DELETE /v1/rooms/persistent/{key}/attachments/{attachment_id}` — take it
/// back out.
///
/// Row first, bytes second, and only after the commit: the row is what makes a
/// blob reachable, so removing it first means a crash between the two leaves an
/// unreferenced file rather than a live row pointing at nothing.
pub(super) async fn room_delete_attachment(
    State(state): State<AppState>,
    Path((key, attachment_id)): Path<(String, String)>,
    Query(query): Query<DeleteAttachmentQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = RoomKey::new(key.trim());
    let id = attachment_id.trim();
    let Some(path) = blob_path(state.room_attachments_root.as_path(), &key, id) else {
        return malformed_attachment_id_response();
    };
    let actor = query.actor_id.trim();
    if actor.is_empty() {
        return invalid_request_response();
    }
    // Same gate as the upload: a client must not delete a room's file while
    // claiming to be one of its agents or the daemon itself.
    if let Some(refusal) = forged_author_response(&state, &key, actor) {
        return refusal;
    }
    let result = with_rooms(&state, |store| {
        store.remove_attachment(&key, id, actor, Utc::now())
    });
    match result {
        Ok((removed, message)) => {
            // Best effort, post-commit. A failed unlink leaves an unreachable
            // file; a pre-commit unlink would risk deleting bytes for a row that
            // then failed to be removed.
            let _ = std::fs::remove_file(&path);
            publish_room_wake(&state, &key, &message);
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "removed": removed })),
            )
        }
        Err(e) => room_store_error_response(e),
    }
}

// ---- Internals --------------------------------------------------------------

/// Write one blob durably: temp file, fsync, owner-only mode, atomic rename.
///
/// The rename is what makes the final path either absent or complete — a reader
/// can never observe a half-written file under an id the store has committed.
/// The fsync happens before the rename so a crash cannot leave a
/// correctly-named file full of zeroes.
fn write_blob(dir: &std::path::Path, path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-only, matching the posture `ocean-store` enforces on `rooms.db`:
        // a room's files are no more public than the index that lists them.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(body)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// One attachment's stored bytes, checked against the row that indexes them.
///
/// The row is the authority and the disk is a cache, so length and hash are
/// re-verified on every read: a truncated, swapped, or half-written file reads
/// as ABSENT rather than being handed back as if it were the thing that was
/// uploaded. That is O(n) with n bounded by [`MAX_ATTACHMENT_BYTES`].
///
/// `None` covers "unreadable" and "does not match" alike, because they mean the
/// same thing to every caller; the two are distinguished in the log, which is
/// where an operator needs them apart.
fn read_verified_blob(
    path: &std::path::Path,
    key: &RoomKey,
    row: &ocean_core::RoomAttachment,
) -> Option<Vec<u8>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(room = %key, attachment = %row.id, error = %e,
                "room attachment row has no readable bytes");
            return None;
        }
    };
    if bytes.len() as u64 != row.byte_len || format!("{:x}", Sha256::digest(&bytes)) != row.sha256 {
        tracing::warn!(room = %key, attachment = %row.id,
            "room attachment bytes disagree with the indexed row");
        return None;
    }
    Some(bytes)
}

/// Put bytes under an attachment id the way an upload would.
///
/// Test-only, and for sibling modules whose fixtures need a room whose files
/// really exist on disk — the convene tests in `persistent_rooms`. It is routed
/// through the same `blob_path`/`write_blob` the upload handler uses, because a
/// fixture that invented its own directory layout would keep passing after the
/// real one moved.
#[cfg(test)]
pub(super) fn write_blob_for_test(root: &std::path::Path, key: &RoomKey, id: &str, bytes: &[u8]) {
    let path = blob_path(root, key, id).expect("a test attachment id must be well-formed");
    write_blob(&room_dir(root, key), &path, bytes).expect("test blob write");
}

/// The same verified read, addressed by room and row instead of by path.
///
/// This is the whole in-process surface: `room_context` assembles a convened
/// agent's prompt out of these bytes and must not re-derive the hashed room
/// directory or re-implement the id check, because a second derivation is a
/// second place for the traversal defence to be got wrong. One derivation, one
/// verification, one place to fix.
pub(super) fn attachment_bytes(
    root: &std::path::Path,
    key: &RoomKey,
    row: &ocean_core::RoomAttachment,
) -> Option<Vec<u8>> {
    read_verified_blob(&blob_path(root, key, &row.id)?, key, row)
}

/// The row exists but its bytes do not, or they no longer match what was
/// recorded. That is a server fault, not a client one: the caller asked for a
/// file the room says it has.
fn bytes_missing_response(key: &RoomKey, id: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "ok": false,
            "code": "attachment_bytes_missing",
            "error": format!("room '{key}' lists attachment '{id}' but its bytes are unreadable"),
        })),
    )
        .into_response()
}

fn internal_error(message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "ok": false, "error": message })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::fake_convene_state;
    use http_body_util::BodyExt;

    /// One open room with a Human and an Agent on the roster. Written straight
    /// through the store rather than over the join route: these tests are about
    /// the attachment handlers, and routing the fixture through another
    /// endpoint's validation would make a change there fail here for no reason.
    fn room_with_roster(state: &AppState, key: &RoomKey) {
        with_rooms(state, |store| {
            store
                .create(key.clone(), key.as_str(), None, Utc::now())
                .expect("room fixture");
            for (id, name, kind) in [
                ("alice", "Alice", RoomParticipantKind::Human),
                ("researcher", "Researcher", RoomParticipantKind::Agent),
            ] {
                store
                    .add_participant(
                        key,
                        ocean_core::RoomParticipant {
                            id: id.into(),
                            kind,
                            display_name: name.into(),
                        },
                        Utc::now(),
                    )
                    .expect("roster fixture");
            }
        });
    }

    async fn upload(
        state: &AppState,
        key: &RoomKey,
        filename: &str,
        content_type: &str,
        uploader: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let (status, Json(body)) = room_upload_attachment(
            State(state.clone()),
            Path(key.as_str().to_string()),
            Query(UploadAttachmentQuery {
                filename: filename.into(),
                content_type: content_type.into(),
                uploader_id: uploader.into(),
            }),
            Bytes::from(body),
        )
        .await;
        (status, body)
    }

    async fn list(state: &AppState, key: &RoomKey) -> serde_json::Value {
        let (status, Json(body)) =
            room_list_attachments(State(state.clone()), Path(key.as_str().to_string())).await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    /// Every file the daemon wrote anywhere under the injected root, so a test
    /// can assert both "nothing was written" and "it landed inside the hashed
    /// directory, not next to it".
    fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Finding B, ported: `uploader_id` is caller-asserted and only
    /// roster-checked, so without the forged-author gate a hostile local caller
    /// could attach a file AS somebody's agent. The artifact route already has
    /// this rule and it is mutation-tested; without the same test here the rule
    /// has a hole the moment attachments exist.
    /// Mutation: delete the `forged_author_response` call -> RED.
    #[tokio::test]
    async fn a_client_cannot_upload_an_attachment_as_an_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("forge-attachment");
        room_with_roster(&state, &key);

        let (status, body) = upload(
            &state,
            &key,
            "notes.md",
            "text/markdown",
            "researcher",
            b"agent said so".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("forged_attachment_author"));

        let listed = list(&state, &key).await;
        assert_eq!(
            listed["attachments"].as_array().map(|a| a.len()),
            Some(0),
            "a forged attachment must not exist"
        );
        assert!(
            files_under(state.room_attachments_root.as_path()).is_empty(),
            "a forged attachment must not leave bytes behind either"
        );

        // The same route still works for a human on the roster.
        let (status, _) = upload(
            &state,
            &key,
            "notes.md",
            "text/markdown",
            "alice",
            b"a human said so".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// The traversal gate. The id in the URL is the blob's filename on disk, so
    /// a caller who can steer it can steer the read. Every hostile shape must be
    /// refused BEFORE any filesystem call, and nothing outside the room's own
    /// hashed directory may ever be touched.
    /// Mutation: make `blob_path` return `Some(dir.join(id))` unconditionally ->
    /// RED.
    #[tokio::test]
    async fn an_attachment_id_from_the_url_cannot_escape_the_room_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("traversal");
        room_with_roster(&state, &key);
        // A real neighbour file the traversal would be aiming at.
        let root = state.room_attachments_root.clone();
        std::fs::create_dir_all(root.as_path()).unwrap();
        let secret = root.parent().unwrap().join("rooms.db");
        std::fs::write(&secret, b"not yours").unwrap();

        for hostile in [
            "../../rooms.db".to_string(),
            "..%2f..%2frooms.db".to_string(),
            "../".repeat(8) + "etc/passwd",
            "0123456789ABCDEF0123456789ABCDEF".to_string(), // right length, wrong case
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_string(), // right length, not hex
            "0123456789abcdef".to_string(),                 // hex, too short
        ] {
            let response = room_download_attachment(
                State(state.clone()),
                Path((key.as_str().to_string(), hostile.clone())),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "hostile id {hostile:?} must be refused on shape"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], json!("malformed_attachment_id"));

            // The delete path validates the same way, before it can unlink.
            let (status, Json(body)) = room_delete_attachment(
                State(state.clone()),
                Path((key.as_str().to_string(), hostile.clone())),
                Query(DeleteAttachmentQuery {
                    actor_id: "alice".into(),
                }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["code"], json!("malformed_attachment_id"));
        }

        assert!(secret.exists(), "the neighbour file must be untouched");
        assert_eq!(std::fs::read(&secret).unwrap(), b"not yours");
        assert!(
            files_under(root.as_path()).is_empty(),
            "no traversal attempt may create anything under the attachment root"
        );
    }

    /// A room key is an unvalidated free-form string (`RoomKey::new` is
    /// `Self(value.into())`), so guarding only the attachment id and then joining
    /// the raw key would reintroduce traversal one level up. The directory is
    /// derived from a hash, so a hostile key is just another 64-hex name.
    /// Mutation: make `room_dir` join `key.as_str()` -> RED.
    #[tokio::test]
    async fn a_hostile_room_key_never_becomes_a_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("../../../etc/hostile");
        room_with_roster(&state, &key);

        let (status, body) = upload(
            &state,
            &key,
            "spec.md",
            "text/markdown",
            "alice",
            b"contents".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");

        let root = state.room_attachments_root.clone();
        let files = files_under(root.as_path());
        assert_eq!(files.len(), 1, "exactly one blob, under the root");
        let parent = files[0].parent().unwrap();
        assert_eq!(
            parent.parent().unwrap(),
            root.as_path(),
            "the blob's directory must be an immediate child of the root"
        );
        let dir_name = parent.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            dir_name.len(),
            64,
            "the room directory is a sha256 hex name"
        );
        assert!(dir_name.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// The cap has to refuse, not truncate, and it has to refuse before writing
    /// anything — otherwise an oversized upload costs disk on its way to a 413.
    /// Mutation: delete the `body.len() > MAX_ATTACHMENT_BYTES` arm -> RED.
    #[tokio::test]
    async fn an_oversized_upload_is_refused_and_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("too-big");
        room_with_roster(&state, &key);

        let (status, body) = upload(
            &state,
            &key,
            "huge.bin",
            "application/octet-stream",
            "alice",
            vec![7u8; MAX_ATTACHMENT_BYTES + 1],
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["code"], json!("attachment_too_large"));
        assert_eq!(body["max_bytes"], json!(MAX_ATTACHMENT_BYTES));

        let listed = list(&state, &key).await;
        assert_eq!(listed["attachments"].as_array().map(|a| a.len()), Some(0));
        assert!(
            files_under(state.room_attachments_root.as_path()).is_empty(),
            "an oversized upload must not cost a byte of disk"
        );

        // Exactly at the cap is allowed — the boundary is inclusive.
        let (status, _) = upload(
            &state,
            &key,
            "exact.bin",
            "application/octet-stream",
            "alice",
            vec![7u8; MAX_ATTACHMENT_BYTES],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// The brief's last sentence, made executable: the declaration is recorded
    /// and never acted on. Reflecting an uploader-chosen `text/html` on a
    /// download would be stored XSS against ocean-surface, which the daemon
    /// serves.
    /// Mutation: echo `row.content_type` in the response `Content-Type` -> RED.
    #[tokio::test]
    async fn a_declared_content_type_is_recorded_but_never_served() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("declared-type");
        room_with_roster(&state, &key);

        let (status, body) = upload(
            &state,
            &key,
            "payload.html",
            "text/html",
            "alice",
            b"<script>alert(1)</script>".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["attachment"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            body["attachment"]["content_type"],
            json!("text/html"),
            "the declaration IS recorded — a client can still show a sensible icon"
        );

        let response =
            room_download_attachment(State(state.clone()), Path((key.as_str().to_string(), id)))
                .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream",
            "the declared type must never be echoed back"
        );
        assert_eq!(
            response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
    }

    /// The narrowing of rule 2, made executable. Every payload here is uploaded
    /// DECLARED `application/octet-stream` and comes back as something more
    /// specific, which only a derivation from the bytes can do — so this passing
    /// while the declared-type test above also passes is the whole rule: the
    /// declaration is ignored, the bytes are not.
    /// Mutation: return `None` unconditionally from `sniff_image_content_type`
    /// -> RED.
    #[tokio::test]
    async fn an_image_is_served_the_type_its_own_bytes_prove() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("sniffed-images");
        room_with_roster(&state, &key);

        for (filename, bytes, expected) in [
            (
                "shot.png",
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR....".to_vec(),
                "image/png",
            ),
            (
                "shot.jpg",
                b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01".to_vec(),
                "image/jpeg",
            ),
            (
                "shot.gif",
                b"GIF89a\x01\x00\x01\x00\x00\x00".to_vec(),
                "image/gif",
            ),
            (
                "shot.webp",
                b"RIFF\x1a\x00\x00\x00WEBPVP8 ....".to_vec(),
                "image/webp",
            ),
        ] {
            let (status, body) = upload(
                &state,
                &key,
                filename,
                "application/octet-stream",
                "alice",
                bytes.clone(),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            let id = body["attachment"]["id"].as_str().unwrap().to_string();

            let response = room_download_attachment(
                State(state.clone()),
                Path((key.as_str().to_string(), id)),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                expected,
                "{filename} must be served the type its own bytes prove"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .unwrap(),
                "nosniff",
                "nosniff is what keeps a derived type from being an escalation"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_DISPOSITION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                format!("attachment; filename=\"{filename}\""),
                "an <img src> renders regardless, so the navigation defence stays"
            );
            let served = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(served.as_ref(), bytes.as_slice());
        }
    }

    /// The mirror of the declared-type test, and the reason the allowlist is
    /// closed: DECLARING `image/png` buys nothing, because only the bytes are
    /// consulted and none of these are on the list. SVG is the one that matters
    /// most — it is the scriptable image format, so it must fall through here
    /// forever.
    /// Mutation: fall back to `row.content_type` instead of octet-stream -> RED.
    #[tokio::test]
    async fn bytes_that_are_not_an_image_stay_octet_stream_however_they_are_declared() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("not-an-image");
        room_with_roster(&state, &key);

        for (filename, declared, bytes) in [
            // Declared an image; the bytes are script.
            (
                "liar.png",
                "image/png",
                b"<script>alert(1)</script>".to_vec(),
            ),
            // Two bytes: a truncated prefix of the PNG signature, and shorter
            // than every signature on the list.
            ("truncated.png", "image/png", b"\x89P".to_vec()),
            // `RIFF` with something other than `WEBP` in the gap.
            (
                "clip.avi",
                "video/x-msvideo",
                b"RIFF\x24\x00\x00\x00AVI LIST".to_vec(),
            ),
            // Scriptable, and signature-less: it could not be admitted even if
            // someone wanted to.
            (
                "logo.svg",
                "image/svg+xml",
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>".to_vec(),
            ),
        ] {
            let (status, body) =
                upload(&state, &key, filename, declared, "alice", bytes.clone()).await;
            assert_eq!(status, StatusCode::CREATED);
            let id = body["attachment"]["id"].as_str().unwrap().to_string();

            let response = room_download_attachment(
                State(state.clone()),
                Path((key.as_str().to_string(), id)),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/octet-stream",
                "{filename} declared {declared} is not on the allowlist"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .unwrap(),
                "nosniff"
            );
            let served = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(served.as_ref(), bytes.as_slice());
        }
    }

    /// Two ways to get a magic-byte check wrong: match too eagerly, or index
    /// past the end of a short file. `RIFF` without `WEBP` in the gap is the
    /// first — every WAV and AVI opens with it. An eleven-byte `RIFF` header is
    /// the second: one byte short of the window the WebP arm reads, and the only
    /// input in this module that can panic a handler rather than merely answer
    /// wrongly.
    /// Mutation: drop the `bytes.len() >= 12` guard -> the eleven-byte case
    /// panics -> RED.
    #[test]
    fn the_derived_type_allowlist_is_closed_and_never_reads_past_the_end() {
        assert_eq!(
            sniff_image_content_type(b"GIF87a\x01\x00"),
            Some("image/gif"),
            "the older GIF version is on the list too"
        );
        assert_eq!(sniff_image_content_type(b""), None);
        assert_eq!(sniff_image_content_type(b"RIFF"), None);
        assert_eq!(
            sniff_image_content_type(b"RIFF\x00\x00\x00\x00WEB"),
            None,
            "eleven bytes is one short of the WebP window"
        );
        assert_eq!(
            sniff_image_content_type(b"RIFF\x00\x00\x00\x00WAVEfmt "),
            None,
            "a WAV opens with RIFF and is not an image"
        );
        assert_eq!(
            sniff_image_content_type(b"\x89PN"),
            None,
            "a truncated signature is not a match"
        );
    }

    /// The product claim: what goes in comes back out, byte for byte, with the
    /// room's history saying it happened.
    #[tokio::test]
    async fn a_downloaded_attachment_round_trips_its_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("round-trip");
        room_with_roster(&state, &key);
        let contents: Vec<u8> = (0u8..=255).cycle().take(50_000).collect();

        let (status, body) = upload(
            &state,
            &key,
            "spec.pdf",
            "application/pdf",
            "alice",
            contents.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["attachment"]["id"].as_str().unwrap().to_string();
        assert_eq!(body["attachment"]["byte_len"], json!(contents.len()));

        let listed = list(&state, &key).await;
        assert_eq!(listed["attachments"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(listed["attachments"][0]["filename"], json!("spec.pdf"));

        let response = room_download_attachment(
            State(state.clone()),
            Path((key.as_str().to_string(), id.clone())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"spec.pdf\""
        );
        let served = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(served.as_ref(), contents.as_slice());

        // The room explains itself without anyone reading the attachment list.
        let transcript = with_rooms(&state, |store| store.transcript(&key, None)).unwrap();
        assert!(
            transcript
                .iter()
                .any(|m| m.body.contains("spec.pdf") && m.body.contains("50000")),
            "the transcript must record the attachment: {transcript:?}"
        );
    }

    /// The same round trip with a name a header cannot spell. The status was
    /// always 200, so asserting it proves nothing — the header is the subject.
    /// What the handler used to send was raw UTF-8 in a parameter that declares
    /// no charset, which two of the three major browsers save as `cafÃ©.png`.
    /// Mutation: `format!` `row.filename` into the header -> RED on the value.
    #[tokio::test]
    async fn a_non_ascii_filename_still_gets_a_content_disposition() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("accented");
        room_with_roster(&state, &key);

        // Decomposed: `e` plus a combining acute. The accent is a code point
        // `HeaderValue::from_str` refuses outright.
        let filename = "cafe\u{301}.png";
        let bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR....".to_vec();
        let (status, body) =
            upload(&state, &key, filename, "image/png", "alice", bytes.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let id = body["attachment"]["id"].as_str().unwrap().to_string();
        assert_eq!(
            body["attachment"]["filename"],
            json!(filename),
            "the row keeps the name the uploader gave it; only the header encodes"
        );

        let response =
            room_download_attachment(State(state.clone()), Path((key.as_str().to_string(), id)))
                .await;
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .expect("a name this header cannot spell must cost the spelling, not the header")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            disposition, "attachment; filename=\"cafe_.png\"; filename*=UTF-8''cafe%CC%81.png",
            "a client that ignores filename* still gets a usable .png name"
        );
        assert!(
            disposition.starts_with("attachment;"),
            "the top-level-navigation defence is the point of the header"
        );

        // The sniffed-image branch is the one that leans on it hardest.
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        let served = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(served.as_ref(), bytes.as_slice());
    }

    /// A mis-uploaded file must be removable: row gone, bytes gone, and the room
    /// able to say who removed it.
    /// Mutation: delete the `remove_file` call -> the bytes survive -> RED.
    #[tokio::test]
    async fn deleting_an_attachment_removes_the_row_the_bytes_and_leaves_a_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("removable");
        room_with_roster(&state, &key);

        let (_, body) = upload(
            &state,
            &key,
            "oops.png",
            "image/png",
            "alice",
            b"wrong file".to_vec(),
        )
        .await;
        let id = body["attachment"]["id"].as_str().unwrap().to_string();
        assert_eq!(files_under(state.room_attachments_root.as_path()).len(), 1);

        let (status, body) = room_delete_attachment(
            State(state.clone()),
            Path((key.as_str().to_string(), id.clone())),
            Query(DeleteAttachmentQuery {
                actor_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["removed"]["filename"], json!("oops.png"));

        let listed = list(&state, &key).await;
        assert_eq!(listed["attachments"].as_array().map(|a| a.len()), Some(0));
        assert!(
            files_under(state.room_attachments_root.as_path()).is_empty(),
            "the bytes must go with the row"
        );
        let transcript = with_rooms(&state, |store| store.transcript(&key, None)).unwrap();
        assert!(
            transcript
                .iter()
                .any(|m| m.body.contains("alice") && m.body.contains("removed")),
            "the transcript must record who removed it: {transcript:?}"
        );

        // A second delete is a typed 404, not a silent success.
        let (status, body) = room_delete_attachment(
            State(state.clone()),
            Path((key.as_str().to_string(), id)),
            Query(DeleteAttachmentQuery {
                actor_id: "alice".into(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["ok"], json!(false));
    }

    /// The bytes-before-row order has one obligation: when the row fails, the
    /// bytes do not linger. Here the room exists for the pre-check but the
    /// uploader is not on the roster, so the store refuses inside its
    /// transaction — after the blob is already on disk.
    /// Mutation: delete the `remove_file` in the `Err` arm -> an orphan blob
    /// survives -> RED.
    #[tokio::test]
    async fn a_store_failure_after_the_blob_write_leaves_no_orphan_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("orphan-check");
        room_with_roster(&state, &key);

        let (status, _) = upload(
            &state,
            &key,
            "spec.md",
            "text/markdown",
            "stranger",
            b"who are you".to_vec(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a non-roster uploader is refused by the store"
        );
        let listed = list(&state, &key).await;
        assert_eq!(listed["attachments"].as_array().map(|a| a.len()), Some(0));
        assert!(
            files_under(state.room_attachments_root.as_path()).is_empty(),
            "a refused row must not leave its bytes behind"
        );
    }

    /// A download must never serve bytes the row does not vouch for. The row is
    /// the authority; the disk is a cache that can be truncated, swapped, or
    /// half-written.
    /// Mutation: delete the length/sha comparison -> the tampered bytes are
    /// served with a 200 -> RED.
    #[tokio::test]
    async fn tampered_bytes_are_refused_rather_than_served() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = fake_convene_state(&tmp);
        let key = RoomKey::new("tampered");
        room_with_roster(&state, &key);

        let (_, body) = upload(
            &state,
            &key,
            "spec.md",
            "text/markdown",
            "alice",
            b"the real spec".to_vec(),
        )
        .await;
        let id = body["attachment"]["id"].as_str().unwrap().to_string();

        let files = files_under(state.room_attachments_root.as_path());
        assert_eq!(files.len(), 1);
        std::fs::write(&files[0], b"the fake spec").unwrap(); // same length, different bytes

        let response =
            room_download_attachment(State(state.clone()), Path((key.as_str().to_string(), id)))
                .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], json!("attachment_bytes_missing"));
    }

    /// A filename is display-only, but it still reaches a header and a transcript
    /// line, so it must not be able to carry a control character or a quote into
    /// either. `.`/`..` are refused outright even though the filename never
    /// becomes a path — cheap, and it keeps the value meaningless as a path if
    /// someone later mistakes it for one.
    #[test]
    fn a_filename_cannot_carry_a_path_a_quote_or_a_control_character() {
        assert_eq!(sanitize_filename("spec.md").as_deref(), Some("spec.md"));
        assert_eq!(
            sanitize_filename("/etc/passwd").as_deref(),
            Some("passwd"),
            "a directory prefix is dropped, not honoured"
        );
        assert_eq!(
            sanitize_filename("C:\\Users\\x\\spec.md").as_deref(),
            Some("spec.md")
        );
        assert_eq!(
            sanitize_filename("a\r\nX-Evil: 1").as_deref(),
            Some("aX-Evil: 1"),
            "control characters are stripped so a header cannot be split"
        );
        assert_eq!(
            sanitize_filename("a\"; filename=\"b").as_deref(),
            Some("a; filename=b"),
            "quotes cannot break out of the Content-Disposition parameter"
        );
        assert_eq!(sanitize_filename("..").as_deref(), None);
        assert_eq!(sanitize_filename(".").as_deref(), None);
        assert_eq!(sanitize_filename("").as_deref(), None);
        assert_eq!(sanitize_filename("   ").as_deref(), None);
        assert_eq!(
            sanitize_filename(&"x".repeat(500)).map(|f| f.len()),
            Some(MAX_FILENAME_LEN)
        );
    }

    /// `sanitize_filename` keeps non-ASCII on purpose, so the header builder is
    /// the only thing between `café.png` and a header no client reads back as
    /// `café.png`. Every name must produce a header, that header must be
    /// unambiguous ASCII, and the UTF-8 name must round-trip through
    /// `filename*`.
    /// Mutation: drop the `filename*` parameter -> the round trip -> RED.
    /// Mutation: `format!` the name into `filename=` -> raw obs-text, `to_str`
    /// fails -> RED.
    #[test]
    fn a_filename_a_header_cannot_spell_still_produces_one() {
        let id = "0123456789abcdef0123456789abcdef";

        // The plain case is unchanged: one parameter, no encoding theatre.
        assert_eq!(
            content_disposition("spec.pdf", id),
            "attachment; filename=\"spec.pdf\""
        );
        assert_eq!(
            content_disposition("q3 notes.md", id),
            "attachment; filename=\"q3 notes.md\"",
            "a space is legal inside the quoted-string and needs no escaping"
        );

        // The canonical break: a decomposed `é` is `e` plus a combining acute.
        // `HeaderValue` ACCEPTS those bytes — they are RFC 9110 obs-text — which
        // is precisely why formatting is not good enough: the value reaches the
        // wire carrying a charset it never declares, and Firefox and Chrome read
        // it as Latin-1. `to_str` refusing it is that ambiguity, made visible.
        let decomposed = "cafe\u{301}.png";
        let naive = HeaderValue::from_str(&format!("attachment; filename=\"{decomposed}\""))
            .expect("obs-text is accepted, so the naive version fails silently, not loudly");
        assert!(
            naive.to_str().is_err(),
            "the naive header is not text any client can agree on"
        );

        // A byte `from_str` really does refuse. This is the arm the old
        // `if let Ok(..)` swallowed, dropping the header entirely.
        assert!(
            HeaderValue::from_str("attachment; filename=\"a\u{7f}b\"").is_err(),
            "0x7f is the boundary the omission path lived on"
        );
        assert_eq!(
            content_disposition("a\u{7f}b.png", id),
            "attachment; filename=\"a_b.png\"; filename*=UTF-8''a%7Fb.png",
            "a refused byte must cost the byte, not the header"
        );

        let header = content_disposition(decomposed, id);
        let value = header.to_str().expect("the header must be visible ASCII");
        assert_eq!(
            value, "attachment; filename=\"cafe_.png\"; filename*=UTF-8''cafe%CC%81.png",
            "the ASCII parameter keeps the extension, the encoded one keeps the name"
        );

        // `filename*` is only worth emitting if it round-trips.
        let encoded = value.split("filename*=UTF-8''").nth(1).unwrap();
        assert_eq!(percent_decode(encoded), decomposed.as_bytes());

        // Precomposed, a name with no ASCII at all, and one carrying a literal
        // `%` that must not be mistaken for an escape it did not write.
        for name in [
            "café.png",
            "日本語.png",
            "√",
            "100% café.txt",
            "naïve v2.pdf",
        ] {
            let header = content_disposition(name, id);
            let value = header.to_str().expect("visible ASCII");
            let encoded = value
                .split("filename*=UTF-8''")
                .nth(1)
                .unwrap_or_else(|| panic!("{name} must carry filename*: {value}"));
            assert_eq!(
                percent_decode(encoded),
                name.as_bytes(),
                "{name} must survive the encoding"
            );
        }

        // A row holding a name with nothing spellable left in it at all still
        // has to name the blob.
        assert_eq!(
            content_disposition("", id),
            format!("attachment; filename=\"{id}\""),
            "the id is the last resort the module has always promised"
        );
        assert_eq!(
            content_disposition("   ", id),
            format!("attachment; filename=\"{id}\"")
        );
    }

    /// Undoes `content_disposition`'s RFC 5987 encoding. Test-only: nothing in
    /// the daemon consumes this header, so a decoder in the module itself would
    /// be dead code kept alive by its own test.
    fn percent_decode(value: &str) -> Vec<u8> {
        let bytes = value.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        out
    }

    /// The declared type is stored, so its SHAPE is the only thing that has to
    /// be bounded — an unbounded or newline-bearing value would ride into logs
    /// and rendered views.
    #[test]
    fn a_declared_content_type_must_be_bounded_visible_ascii() {
        assert!(is_storable_content_type("text/markdown"));
        assert!(is_storable_content_type("text/plain; charset=utf-8"));
        assert!(!is_storable_content_type(""));
        assert!(!is_storable_content_type("text/html\nX-Evil: 1"));
        assert!(!is_storable_content_type(
            &"x".repeat(MAX_CONTENT_TYPE_LEN + 1)
        ));
    }

    /// Minted ids satisfy the validator that guards the filesystem. If these two
    /// ever disagree, every upload 500s — which is the safe direction, but this
    /// keeps it from happening at all.
    #[test]
    fn every_minted_id_passes_the_path_validator() {
        for _ in 0..64 {
            let id = mint_attachment_id();
            assert!(is_attachment_id(&id), "minted a bad id: {id}");
        }
    }
}
