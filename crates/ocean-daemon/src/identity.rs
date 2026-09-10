//! Rooms S0 — `GET /v1/identity`: who this daemon says its human is.
//!
//! Design direction §3.2 (`ocean-surface/docs/OCEAN_ROOMS_DESIGN_DIRECTION.md`):
//! one human = one member id on every host. The daemon publishes the same
//! string `ocean-mcp` resolves, from the same file, so a terminal, the desktop
//! app, the Chrome extension, and the browser (through the proxy's cross-check)
//! converge on one id without any client inventing one.
//!
//! Sources, in order:
//!
//! 1. `<config_dir>/member.toml` — `member_id = "..."`, optional
//!    `display_name = "..."`; the config dir is the daemon's own
//!    (`OCEAN_CONFIG_DIR`, `XDG_CONFIG_HOME/ocean-rs`, then
//!    `~/.config/ocean-rs`), the directory `operator.key` and `rooms.db`
//!    already live in.
//! 2. `OCEAN_MEMBER_ID`.
//!
//! Neither set → `member_id: null`, `source: "unset"`. NEVER the process user:
//! a daemon that does not know who you are says so instead of guessing, and a
//! client that receives `null` stays read-only until someone writes the file.
//!
//! Credential-free, and read at request time rather than at startup, so
//! writing `member.toml` takes effect without a daemon restart.

use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

pub(super) const MEMBER_FILE: &str = "member.toml";

/// Where the answer came from. The wire strings are the ones the design
/// direction fixes (`member.toml` | `env` | `unset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) enum IdentitySource {
    #[serde(rename = "member.toml")]
    MemberToml,
    #[serde(rename = "env")]
    Env,
    #[serde(rename = "unset")]
    Unset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Identity {
    pub member_id: Option<String>,
    pub display_name: Option<String>,
    pub source: IdentitySource,
}

impl Identity {
    fn unset() -> Self {
        Self {
            member_id: None,
            display_name: None,
            source: IdentitySource::Unset,
        }
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "member_id": self.member_id,
            "display_name": self.display_name,
            "source": self.source,
        })
    }
}

/// `GET /v1/identity`.
pub(super) async fn identity() -> Json<Value> {
    let config_dir = ocean_agent::config_dir_from_env();
    let env_member = std::env::var("OCEAN_MEMBER_ID").ok();
    Json(resolve(&config_dir, env_member.as_deref()).to_json())
}

/// The pure resolver: `member.toml` in `config_dir`, then the env value the
/// caller already read. Anything malformed is treated as absent — the file is
/// never "repaired" into an id, and a bad `member_id` line does not stop the
/// env fallback.
pub(super) fn resolve(config_dir: &Path, env_member: Option<&str>) -> Identity {
    if let Some(parsed) = parse_member_toml(&config_dir.join(MEMBER_FILE)) {
        return parsed;
    }
    match env_member.map(str::trim).filter(|m| valid_member_id(m)) {
        Some(member_id) => Identity {
            member_id: Some(member_id.to_string()),
            display_name: None,
            source: IdentitySource::Env,
        },
        None => Identity::unset(),
    }
}

/// The same character set `ocean-mcp` accepts (`member_from_toml`), so both
/// readers of one file always agree on whether it names anyone.
fn valid_member_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn valid_display_name(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 80 && !value.chars().any(char::is_control)
}

/// One `key = "value"` per line is all the file may hold; `#` comments and
/// blank lines are skipped, a trailing `# comment` after the closing quote is
/// allowed. No TOML crate: the file has two keys and the parser must stay
/// byte-for-byte predictable across the daemon and `ocean-mcp`.
fn parse_member_toml(path: &Path) -> Option<Identity> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut member_id = None;
    let mut display_name = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(rest);
        match key.trim() {
            "member_id" if valid_member_id(value) => member_id = Some(value.to_string()),
            "display_name" if valid_display_name(value) => display_name = Some(value.to_string()),
            _ => {}
        }
    }
    member_id.map(|member_id| Identity {
        member_id: Some(member_id),
        display_name,
        source: IdentitySource::MemberToml,
    })
}

/// `"John"   # optional` → `John`; an unquoted value ends at `#`.
fn unquote(rest: &str) -> &str {
    let rest = rest.trim();
    if let Some(inner) = rest.strip_prefix('"') {
        inner.split('"').next().unwrap_or("").trim()
    } else {
        rest.split('#').next().unwrap_or("").trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(contents: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        if let Some(contents) = contents {
            std::fs::write(tmp.path().join(MEMBER_FILE), contents).unwrap();
        }
        tmp
    }

    #[test]
    fn member_toml_wins_and_carries_the_display_name() {
        let tmp = dir_with(Some(
            "# who this box is\nmember_id = \"smaths\"\ndisplay_name = \"John\"   # optional\n",
        ));
        let got = resolve(tmp.path(), Some("someone-else"));
        assert_eq!(got.member_id.as_deref(), Some("smaths"));
        assert_eq!(got.display_name.as_deref(), Some("John"));
        assert_eq!(got.source, IdentitySource::MemberToml);
        assert_eq!(got.to_json()["source"], json!("member.toml"));
    }

    #[test]
    fn env_is_the_fallback_and_is_trimmed() {
        let tmp = dir_with(None);
        let got = resolve(tmp.path(), Some("  ecfromthedc \n"));
        assert_eq!(got.member_id.as_deref(), Some("ecfromthedc"));
        assert_eq!(got.display_name, None);
        assert_eq!(got.source, IdentitySource::Env);
    }

    #[test]
    fn nothing_set_answers_null_and_never_the_process_user() {
        let tmp = dir_with(None);
        let got = resolve(tmp.path(), None);
        assert_eq!(got, Identity::unset());
        let wire = got.to_json();
        assert_eq!(wire["ok"], json!(true));
        assert_eq!(wire["member_id"], Value::Null);
        assert_eq!(wire["display_name"], Value::Null);
        assert_eq!(wire["source"], json!("unset"));
        // The resolver has no path to the OS user at all: with nothing
        // configured the answer is null, whatever `$USER` says.
        if let Ok(user) = std::env::var("USER") {
            assert_ne!(wire["member_id"], json!(user));
        }
    }

    #[test]
    fn malformed_values_are_absent_not_repaired() {
        // A member_id that ocean-mcp would refuse is refused here too, and the
        // env fallback still applies.
        let tmp = dir_with(Some(
            "member_id = \"not a member id\"\ndisplay_name = \"X\"\n",
        ));
        let got = resolve(tmp.path(), Some("fallback"));
        assert_eq!(got.member_id.as_deref(), Some("fallback"));
        assert_eq!(
            got.display_name, None,
            "a display name without a member id is nothing"
        );
        assert_eq!(got.source, IdentitySource::Env);

        // A bad env value with no file is unset, not invented.
        let empty = dir_with(None);
        assert_eq!(resolve(empty.path(), Some("has space")), Identity::unset());
        assert_eq!(resolve(empty.path(), Some("   ")), Identity::unset());

        // A control character or an over-long display name is dropped while
        // the member id stands.
        let tmp = dir_with(Some(
            "member_id = \"jay\"\ndisplay_name = \"bad\u{7}name\"\n",
        ));
        let got = resolve(tmp.path(), None);
        assert_eq!(got.member_id.as_deref(), Some("jay"));
        assert_eq!(got.display_name, None);
        let long = format!(
            "member_id = \"jay\"\ndisplay_name = \"{}\"\n",
            "x".repeat(81)
        );
        let tmp = dir_with(Some(&long));
        assert_eq!(resolve(tmp.path(), None).display_name, None);
    }

    #[test]
    fn parsing_is_line_based_and_tolerates_comments_and_unquoted_values() {
        let tmp = dir_with(Some(
            "\n  # comment line\nmember_id=jake   # unquoted, trailing comment\n[section]\ndisplay_name = Jake B\n",
        ));
        let got = resolve(tmp.path(), None);
        assert_eq!(got.member_id.as_deref(), Some("jake"));
        assert_eq!(got.display_name.as_deref(), Some("Jake B"));
    }
}
