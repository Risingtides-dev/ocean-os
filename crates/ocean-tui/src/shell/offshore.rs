//! Shared offshore-mode flag and per-turn guidance.

use std::{env, fs, path::PathBuf};

const GUIDANCE: &str = "offshore mode is ON: run heavy or subagent-style work on remote compute via the offshore_* tools (offshore_workspace → offshore_dispatch → offshore_events → offshore_ship/offshore_fetch → offshore_clean) instead of doing it inline. One session per job; dispatch prompts must instruct the remote agent to commit its work.";

fn mode_path() -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config").join("offshore").join("mode")
}

pub fn enabled() -> bool {
    fs::read_to_string(mode_path())
        .map(|contents| contents.trim().eq_ignore_ascii_case("on"))
        .unwrap_or(false)
}

pub fn guidance(enabled: bool) -> Option<Vec<String>> {
    enabled.then(|| vec![GUIDANCE.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_only_exists_when_enabled() {
        assert!(guidance(false).is_none());
        assert!(guidance(true).is_some_and(|lines| lines == [GUIDANCE]));
    }
}
