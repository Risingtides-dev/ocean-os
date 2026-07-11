use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ocean_agent::config_dir_from_env;
use ocean_context::{ClaimStatus, Provenance};
use ocean_memory::{
    Memory, MemoryId, MemoryKind, MemoryScope, MemoryStore, PrincipalId, SqliteMemoryStore,
};
use serde_json::json;

struct Seed {
    text: &'static str,
    kind: MemoryKind,
}

const SEEDS: &[Seed] = &[
    Seed {
        text: "ocean-os repo: Rust workspace at ~/dev/ocean-os; daemon on 127.0.0.1:4780 (health at GET /health, NOT /v1/health); TUI binary `ocean`.",
        kind: MemoryKind::Fact,
    },
    Seed {
        text: "Build: cargo build --workspace --release; TUI-only: cargo build -p ocean-tui --release.",
        kind: MemoryKind::Skill,
    },
    Seed {
        text: "Deploy: rebuild from main, then launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon; daemon is the LaunchAgent dev.risingtides.ocean-daemon and must run from a neutral cwd ($HOME), never inside a git repo.",
        kind: MemoryKind::Skill,
    },
    Seed {
        text: "Gates before any commit: cargo check --workspace, cargo fmt --check; clippy -D warnings is CI-gated.",
        kind: MemoryKind::Skill,
    },
    Seed {
        text: "Sessions persist at ~/.config/ocean-rs/sessions/<workspace-bucket>/<id>.json; sessions live in the daemon only.",
        kind: MemoryKind::Fact,
    },
    Seed {
        text: "Ledger: append work entries to events.md at repo root (schema in AGENTS.md); update the nearest AGENTS.md after meaningful changes.",
        kind: MemoryKind::Skill,
    },
    Seed {
        text: "Operator john (Risingtides-dev): bias to action, land verified work to main by default, no permission loops.",
        kind: MemoryKind::Preference,
    },
    Seed {
        text: "Crate map: ocean-core protocol types, ocean-runtime agent loop + tools, ocean-agent session/history + system prompt, ocean-providers model routing, ocean-daemon HTTP service, ocean-tui terminal client, ocean-memory typed SQLite memory store.",
        kind: MemoryKind::Fact,
    },
];

struct SeedOutcome {
    text: &'static str,
    inserted: bool,
}

fn seed_memories(path: &Path) -> anyhow::Result<Vec<SeedOutcome>> {
    let mut store = SqliteMemoryStore::open(path)?;
    let owner = PrincipalId::new("operator");
    let mut existing = HashSet::new();
    let mut after = None;
    loop {
        let page = store.list_page(&owner, after, Some(100))?;
        for memory in &page.memories {
            if let Some(text) = memory.body.get("text").and_then(serde_json::Value::as_str) {
                existing.insert(text.to_owned());
            }
        }
        match page.next_seq {
            Some(seq) if page.has_more => after = Some(seq),
            _ => break,
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX);
    let mut outcomes = Vec::with_capacity(SEEDS.len());
    for seed in SEEDS {
        let inserted = if existing.contains(seed.text) {
            false
        } else {
            store.put(Memory {
                id: MemoryId::new(),
                scope: MemoryScope::Operator,
                owner: owner.clone(),
                kind: seed.kind.clone(),
                body: json!({ "text": seed.text }),
                provenance: Provenance {
                    anchors: Vec::new(),
                    tickets: Vec::new(),
                    commit_sha: String::new(),
                },
                trust: ClaimStatus::Asserted,
                seq: 0,
                written_at: now,
                updated_at: now,
                history: Vec::new(),
            })?;
            existing.insert(seed.text.to_owned());
            true
        };
        outcomes.push(SeedOutcome {
            text: seed.text,
            inserted,
        });
    }
    Ok(outcomes)
}

fn main() -> anyhow::Result<()> {
    let memory_db = config_dir_from_env().join("memory.sqlite");
    if let Some(parent) = memory_db.parent() {
        std::fs::create_dir_all(parent)?;
    }

    for outcome in seed_memories(&memory_db)? {
        let action = if outcome.inserted {
            "inserted"
        } else {
            "skipped"
        };
        println!("{action}: {}", outcome.text);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_is_idempotent_by_exact_text() {
        let dir = tempfile::TempDir::new().expect("memory tempdir");
        let path = dir.path().join("memory.sqlite");

        let first = seed_memories(&path).expect("first seed pass");
        assert_eq!(
            first.iter().filter(|outcome| outcome.inserted).count(),
            SEEDS.len()
        );

        let second = seed_memories(&path).expect("second seed pass");
        assert!(second.iter().all(|outcome| !outcome.inserted));
        assert_eq!(
            ocean_agent::list_memories(&path, SEEDS.len() + 1).len(),
            SEEDS.len()
        );
    }
}
