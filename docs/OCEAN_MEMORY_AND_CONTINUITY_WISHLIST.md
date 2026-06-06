# Ocean Memory & Continuity — Ultimate Wishlist

> *What it would look like if Ocean never forgot, always caught up, and could act on everything it's ever known.*

---

## 1. Conversational Memory (Within a Session)

**Current state:** Ocean remembers everything said in the current conversation turn-by-turn.

**Wishlist:**

| Feature | Description | Priority |
|---|---|---|
| **Scrollable history** | In TUI, scroll back through the full conversation without any cap | P0 |
| **Search history** | `/search "something I said"` finds it in the current session transcript | P1 |
| **Session bookmarking** | Mark a point in the conversation to jump back to later | P2 |

---

## 2. Persistent Memory (Across Sessions)

**Current state:** Zero. Every new conversation starts blank.

**Wishlist — tiers of persistence:**

### Tier 1: Fact Memory (Ephemeral)

A simple JSONL file (`~/.ocean/memory/facts.jsonl`) where Ocean writes and reads atomic facts.

```text
{"ts": "2026-05-25T14:00:00Z", "type": "user_preference", "key": "default_model", "value": "claude-sonnet-4-20250514"}
{"ts": "2026-05-25T14:05:00Z", "type": "project_context", "key": "ocean-os", "value": "Rust-native agentic OS, workspace at ~/dev/ocean-os"}
{"ts": "2026-05-25T14:10:00Z", "type": "decision", "key": "auth_strategy", "value": "local-first, no cloud dependency"}
```

- **Commands:** `/remember <key> = <value>`, `/recall <key>`, `/forget <key>`, `/mem` (list all)
- **Auto-extraction:** After each turn, Ocean writes a summary fact automatically
- **Auto-injection:** On session start, Ocean loads relevant facts into context

### Tier 2: Episodic Memory (Working Context)

A rolling log of session summaries, decisions, and outcomes.

```text
~/.ocean/memory/
├── facts.jsonl          # Tier 1
├── episodes.jsonl       # Tier 2 — session summaries
├── decisions.jsonl      # Tier 2 — key architectural decisions
├── session-{id}.jsonl   # Tier 2 — full transcripts (configurable retention)
└── index.sqlite         # Tier 3 — queryable memory store
```

- **Auto-summarization:** Every session end writes a structured summary
- **Commands:** `/episodes`, `/episode <id>`, `/decisions`
- **Recall by relevance:** "What did we decide about auth?" searches episodes + decisions

### Tier 3: Semantic Memory (Vector Search)

Embedding-based retrieval for fuzzier "I remember something about…" queries.

- **Local embedding model** (e.g., `sentence-transformers` via ONNX or a small GGUF)
- **Vector store** (e.g., `tinyvec` or SQLite with `sqlite-vec`)
- **Commands:** `/find "something about the auth flow"` — returns top-3 matching memories
- **Auto-context injection:** On session start, Ocean pulls top-5 semantically relevant memories

### Tier 4: Procedural Memory (Skill Library)

Ocean remembers *how to do things* — not just facts.

- **Learned workflows:** "Last time we deployed, I ran `cargo build --release && scp target/release/ocean-daemon server:~/ocean/`"
- **Skill learning:** Ocean watches repeated patterns and offers to save them
- **Commands:** `/learn <name>` (saves current workflow), `/show <name>`, `/forget-skill <name>`
- **Auto-suggest:** When context matches a learned skill, Ocean suggests it

---

## 3. Continuity Features

| Feature | Description | Priority |
|---|---|---|
| **Graceful resume** | If daemon restarts, Ocean picks up where it left off — last active session, scroll position, pending permissions | P0 |
| **Session checkpointing** | `/checkpoint` snapshots the full TUI state (input buffer, scroll, selected session, PM turn view) | P1 |
| **State persistence** | TUI saves its view state to `~/.ocean/tui-state.json` on every meaningful action | P1 |
| **Crashtest recovery** | If the TUI or daemon crashes, the other side reconnects and restores state | P2 |
| **Multi-device sync** | Memory is synced across machines (optional, opt-in, encrypted) | P3 |

---

## 4. Memory UI in TUI

| Feature | Description | Priority |
|---|---|---|
| **Memory tab** | A new tab/sidebar in the TUI showing recent facts, episodes, top decisions | P1 |
| **Memory search bar** | Ctrl+F in PM room searches local conversation + memory | P1 |
| **Memory indicators** | Icons/badges showing "2 facts loaded", "3 similar sessions found" | P2 |
| **Memory timeline** | A chronological graph view of all episodes and decisions | P3 |

---

## 5. Privacy & Control

| Feature | Description | Priority |
|---|---|---|
| **Local-only by default** | All memory lives on-device, never leaves without explicit export | P0 |
| **Selective forgetting** | `/forget` with patterns, date ranges, or exact keys | P0 |
| **Memory pause** | `/pause-memory` stops all memory writes for a sensitive conversation | P1 |
| **Encrypted store** | Optional passphrase-encrypted memory file | P2 |
| **Memory export/import** | `/export-memory` produces a portable JSON tarball | P2 |
| **Memory cleanup** | `/clean-memory --older-than 30d` purges old episodes | P2 |

---

## 6. Agent Team Memory (Multi-Agent)

If Ocean spawns sub-agents (Flux, Pixel, Brick, etc.), they should share memory:

| Feature | Description | Priority |
|---|---|---|
| **Shared fact pool** | All agents read/write to the same facts.jsonl | P0 |
| **Agent-specific episodes** | Each agent writes its own episode stream | P1 |
| **Cross-agent recall** | "Pixel, what did Brick decide about the database schema?" | P2 |
| **Handoff summaries** | When one agent hands off to another, it includes a memory context block | P2 |

---

## 7. Implementation Crate Layout

> **Status:** `ocean-memory` is **[Not yet built — roadmap]** — no `crates/ocean-memory` exists in the workspace yet; the layout below is the planned design. The sibling `ocean-store` crate referenced here, by contrast, **is built** (`crates/ocean-store`): a SQLite-backed (`rusqlite`, bundled) durable store for daemon session/room persistence (`SqliteRoomStore`).

Memory will live in a dedicated **`crates/ocean-memory`** workspace crate — not in `ocean-core` (too heavy), not in `ocean-daemon` (blocks CLI/TUI from reading memory without the daemon), not in `ocean-store` (focused on daemon session/room persistence, not user-facing recall).

### Crate tree

```
crates/ocean-memory/
├── Cargo.toml
│   deps: serde, serde_json, chrono, anyhow, uuid (Tier 1)
│       + rusqlite (Tier 2, optional)
│       + tokenizers / candle / ort (Tier 3, optional)
└── src/
    ├── lib.rs              # MemoryStore trait, Fact, FactKind, MemoryConfig
    ├── file.rs             # FileMemoryStore — JSONL backend
    ├── sqlite.rs           # SqliteMemoryStore — indexed backend (future)
    ├── episodic.rs          # Episode + EpisodicStore
    ├── semantic.rs          # SemanticStore with local embeddings (future)
    └── procedural.rs        # Skill library (future)
```

### Dependencies

```toml
# Cargo.toml for ocean-memory (Tier 1)
[package]
name = "ocean-memory"
version.workspace = true
edition.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
anyhow.workspace = true
uuid.workspace = true
```

### Types (`src/lib.rs`)

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What kind of memory fact is this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FactKind {
    /// User-declared preference or fact.
    User,
    /// Auto-extracted by Ocean after a turn.
    Auto,
    /// Recorded by a sub-agent (Flux, Pixel, etc.).
    Agent(String),
}

/// A single atomic memory fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: uuid::Uuid,
    pub ts: DateTime<Utc>,
    pub kind: FactKind,
    pub key: String,
    pub value: String,
    /// Optional tags for filtering / search.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Memory store trait — pluggable backend.
pub trait MemoryStore: Send + Sync {
    /// Persist a fact.
    fn write(&self, fact: &Fact) -> anyhow::Result<()>;
    /// Read a single fact by key (returns newest match).
    fn read(&self, key: &str) -> anyhow::Result<Option<Fact>>;
    /// Search facts by substring match on key or value.
    fn search(&self, query: &str) -> anyhow::Result<Vec<Fact>>;
    /// Delete a fact by key.
    fn delete(&self, key: &str) -> anyhow::Result<()>;
    /// List all facts, newest first.
    fn list_all(&self) -> anyhow::Result<Vec<Fact>>;
    /// Count facts.
    fn count(&self) -> anyhow::Result<usize>;
}

/// Configuration for the memory system.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Root directory for memory files.
    pub root: PathBuf,
    /// Maximum facts before auto-trim (0 = unlimited).
    pub max_facts: usize,
    /// Maximum episodes before auto-trim.
    pub max_episodes: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            root: dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ocean")
                .join("memory"),
            max_facts: 10_000,
            max_episodes: 500,
        }
    }
}
```

### File backend (`src/file.rs`)

```rust
use super::*;
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
    sync::Mutex,
};

/// JSONL-backed memory store. Thread-safe via internal Mutex.
pub struct FileMemoryStore {
    path: PathBuf,
    /// In-memory index: key → newest Fact (avoids scanning whole file on read).
    index: Mutex<HashMap<String, Fact>>,
}

impl FileMemoryStore {
    pub fn new(config: &MemoryConfig) -> anyhow::Result<Self> {
        let path = config.root.join("facts.jsonl");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self {
            path,
            index: Mutex::new(HashMap::new()),
        };
        // Warm the index from disk.
        store.rebuild_index()?;
        Ok(store)
    }

    fn rebuild_index(&self) -> anyhow::Result<()> {
        let mut index = self.index.lock().unwrap();
        index.clear();
        if !self.path.exists() {
            return Ok(());
        }
        let file = fs::File::open(&self.path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            if let Ok(fact) = serde_json::from_str::<Fact>(&line) {
                // Keep newest per key.
                let entry = index.entry(fact.key.clone()).or_insert(fact.clone());
                if fact.ts > entry.ts {
                    *entry = fact;
                }
            }
        }
        Ok(())
    }
}

impl MemoryStore for FileMemoryStore {
    fn write(&self, fact: &Fact) -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(fact)?;
        writeln!(file, "{line}")?;
        self.index.lock().unwrap().insert(fact.key.clone(), fact.clone());
        // Trim oldest entries if over cap.
        self.maybe_trim()?;
        Ok(())
    }

    fn read(&self, key: &str) -> anyhow::Result<Option<Fact>> {
        Ok(self.index.lock().unwrap().get(key).cloned())
    }

    fn search(&self, query: &str) -> anyhow::Result<Vec<Fact>> {
        let q = query.to_ascii_lowercase();
        let mut results: Vec<Fact> = self.index.lock().unwrap()
            .values()
            .filter(|f| {
                f.key.to_ascii_lowercase().contains(&q)
                    || f.value.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        results.sort_by(|a, b| b.ts.cmp(&a.ts));
        Ok(results)
    }

    fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.index.lock().unwrap().remove(key);
        // Rewrite the full file minus the deleted key.
        self.rewrite_file()?;
        Ok(())
    }

    fn list_all(&self) -> anyhow::Result<Vec<Fact>> {
        let mut facts: Vec<Fact> = self.index.lock().unwrap()
            .values().cloned().collect();
        facts.sort_by(|a, b| b.ts.cmp(&a.ts));
        Ok(facts)
    }

    fn count(&self) -> anyhow::Result<usize> {
        Ok(self.index.lock().unwrap().len())
    }
}

impl FileMemoryStore {
    fn rewrite_file(&self) -> anyhow::Result<()> {
        let facts: Vec<Fact> = self.list_all()?;
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        for fact in &facts {
            let line = serde_json::to_string(fact)?;
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    fn maybe_trim(&self) -> anyhow::Result<()> {
        // Trim logic: when count > max_facts, rewrite with only the
        // newest max_facts (by ts). Keeps the index small.
        let count = self.count()?;
        if count <= self.max_facts { return Ok(()); }
        // ... trimming implementation
        Ok(())
    }
}
```

### Daemon endpoints (`/v1/memory`)

```
GET  /v1/memory             → list all facts
GET  /v1/memory/search?q=   → search facts
GET  /v1/memory/:key        → read fact by key
POST /v1/memory             → write fact { key, value, kind?, tags? }
DELETE /v1/memory/:key      → delete fact by key
OPTIONS /v1/memory          → count
```

### TUI slash commands

| Command | Action |
|---------|--------|
| `/remember <key> = <value>` | POST to `/v1/memory` |
| `/recall <key>` | GET `/v1/memory/<key>` and render in transcript |
| `/search <query>` | GET `/v1/memory/search?q=` and render results |
| `/forget <key>` | DELETE `/v1/memory/<key>` |
| `/mem` | GET `/v1/memory` and list all facts |

Tier 1 totals ~400 lines of Rust (type definitions, file backend, daemon routes, TUI commands).

---

## 8. Quick-Win Roadmap

| Step | What | Effort |
|---|---|---|
| 1 | `ocean-memory` crate with `FileMemoryStore` + `Fact` type | ~2h |
| 2 | `/remember` `/recall` `/forget` `/mem` slash commands in TUI | ~1h |
| 3 | Auto-save session summary fact on `/exit` or Ctrl-Q | ~30min |
| 4 | Auto-load relevant facts on session start (grep by project name) | ~30min |
| 5 | Episodic memory: `episodes.jsonl` with auto-summarization | ~2h |
| 6 | Memory tab in TUI | ~3h |
| 7 | Semantic memory with local embeddings | ~1-2 days |
| 8 | Procedural memory (skill learning) | ~3-5 days |
| 9 | Multi-device sync (encrypted) | ~1 week |

---

*"Ocean should remember what you told it last week, pick up where you left off yesterday, and never ask 'what model are you?' twice."*
