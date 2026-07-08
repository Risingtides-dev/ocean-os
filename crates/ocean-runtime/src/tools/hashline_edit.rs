//! `hashline_edit` — content-hash-anchored file edits (W1, OMP port).
//!
//! The model references files by the `[path#HASH]` tag it saw from `read`, plus
//! line-anchored ops (SWAP/DEL/INS). We re-hash the live file: if it matches the
//! tag, ops apply against exact line numbers; if it drifted, we try three
//! zero-fuzz recovery strategies against the session snapshot the tag names;
//! only if all miss do we reject with a "re-read, the tag is stale" message.
//!
//! This is far more reliable than string-replace editing — stale edits are
//! caught before they corrupt, and edit-retry loops mostly disappear.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ocean_hashline::{apply_patch, ApplyError, NoopLoopGuard, Patch, Recovery};
use serde_json::{json, Value};
use tokio::fs;

use crate::tools::path::resolve_against_cwd;
use crate::tools::read::SharedSnapshots;
use crate::types::{AgentTool, AgentToolResult};

/// Shared, session-scoped no-op loop guard. Tracks consecutive byte-identical
/// no-op edits per path across the session's turns; trips to a hard error so a
/// model stuck re-applying the same changeless patch stops looping instead of
/// burning the whole `max_turns` budget.
pub type SharedNoopGuard = Arc<Mutex<NoopLoopGuard>>;

pub struct HashlineEditTool {
    cwd: Option<PathBuf>,
    snapshots: SharedSnapshots,
    guard: SharedNoopGuard,
}

impl HashlineEditTool {
    pub fn new(cwd: Option<PathBuf>, snapshots: SharedSnapshots, guard: SharedNoopGuard) -> Self {
        Self {
            cwd,
            snapshots,
            guard,
        }
    }
}

#[async_trait]
impl AgentTool for HashlineEditTool {
    fn name(&self) -> &str {
        "hashline_edit"
    }
    fn requires_permission(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Apply a hashline patch. Reference a file by the `[path#HASH]` tag from a prior `read`, \
         then line-anchored ops: `SWAP a.=b:` replace lines a..b (body follows, each line \
         prefixed `+`), `DEL a` / `DEL a.=b`, `INS.PRE a:` / `INS.POST a:` / `INS.HEAD:` / \
         `INS.TAIL:`. Stale tags are rejected — re-read to get a fresh tag."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "One or more `[path#HASH]` sections, each followed by ops."
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let patch_src = args
            .get("patch")
            .and_then(|v| v.as_str())
            .ok_or("missing 'patch'")?;

        let patch = Patch::parse(patch_src).map_err(|e| format!("parse hashline patch: {e}"))?;
        if patch.sections.is_empty() {
            return Err("empty patch: no `[path#HASH]` sections".to_string());
        }

        let mut applied: Vec<String> = Vec::new();
        for section in &patch.sections {
            let resolved = resolve_against_cwd(self.cwd.as_deref(), &section.path);
            let current = fs::read_to_string(&resolved)
                .await
                .map_err(|e| format!("read {}: {e}", section.path))?;

            // One-section patch to hand to apply_patch.
            let single = Patch {
                sections: vec![section.clone()],
            };
            let new_text = match apply_patch(&current, &single) {
                Ok(t) => t,
                Err(ApplyError::Mismatch(mm)) => {
                    // Stale tag → try zero-fuzz recovery against the snapshot.
                    let recovered = {
                        let store = self
                            .snapshots
                            .lock()
                            .map_err(|_| "snapshot store poisoned".to_string())?;
                        Recovery::try_recover(&store, section, &current)
                    };
                    match recovered {
                        Some((merged, warn)) => {
                            applied.push(format!("{} (recovered: {warn})", section.path));
                            merged
                        }
                        None => {
                            let hint = if mm.hash_recognized {
                                "the file changed since you read it"
                            } else {
                                "that tag was never seen — re-read the file, don't invent the tag"
                            };
                            return Err(format!(
                                "stale hashline tag for {}: expected {}, live file is {} ({hint}). \
                                 Re-read the file to get a fresh tag.",
                                section.path, mm.expected_file_hash, mm.actual_file_hash
                            ));
                        }
                    }
                }
                Err(e) => return Err(format!("apply {}: {e}", section.path)),
            };

            // No-op loop guard: the patch applied cleanly but changed nothing.
            // Count it (per path, per identical patch); after the tolerance the
            // guard escalates to a hard error so a model stuck re-issuing the
            // same changeless edit breaks out instead of burning `max_turns`.
            // A genuinely changing edit clears the path's counter.
            if new_text == current {
                if let Ok(mut guard) = self.guard.lock() {
                    guard
                        .observe_noop(&section.path, patch_src)
                        .map_err(|e| e.to_string())?;
                }
                applied.push(format!("{} (no-op: file already matched)", section.path));
                continue;
            }
            if let Ok(mut guard) = self.guard.lock() {
                guard.reset(&section.path);
            }

            fs::write(&resolved, &new_text)
                .await
                .map_err(|e| format!("write {}: {e}", section.path))?;

            // Refresh the snapshot so a follow-up edit anchors on the new content.
            if let Ok(mut store) = self.snapshots.lock() {
                let n = new_text.lines().count().max(1);
                store.record(&section.path, &new_text, [(1, n)]);
            }
            if !applied.iter().any(|a| a.starts_with(&section.path)) {
                applied.push(section.path.clone());
            }
        }

        Ok(AgentToolResult::text(format!(
            "hashline_edit applied: {}",
            applied.join(", ")
        )))
    }
}
