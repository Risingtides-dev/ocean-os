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
//! 2. **The declared content type is recorded and never acted on.** Downloads
//!    are always `application/octet-stream` with `X-Content-Type-Options:
//!    nosniff`. The daemon serves browser origins (loopback,
//!    `chrome-extension://`, `tauri://localhost`), so echoing an
//!    uploader-declared `text/html` back on a download is stored XSS against
//!    ocean-surface. The cost is that an image will not render inline from a
//!    `<img src>`; the fix for that is server-side magic-byte sniffing, which is
//!    derived from the bytes rather than the declaration, and is a separate
//!    slice.
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
/// Always `application/octet-stream` + `nosniff`, never the declared type. See
/// the module header: the daemon answers browser origins, so reflecting an
/// uploader-chosen `text/html` here is stored XSS against ocean-surface.
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

    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // The filename has already had control characters and `"` removed by
    // `sanitize_filename` on the way in, so it cannot break out of the quoted
    // parameter. Fall back to the id if a pre-existing row somehow holds a
    // filename this header cannot carry.
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", row.filename))
    {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
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
