//! W0 — effective per-turn harness-profile seam.
//!
//! A turn's `client_type` selects only the two behaviors this seam actually
//! controls today:
//!
//! - hashline-tagged reads plus `hashline_edit`;
//! - oversized tool-result spill to `artifact://`.
//!
//! LSP and memory providers are registered globally and are not profile-gated.
//! Stream rules, rich-context collection, and a command/context minimizer are
//! not wired, so they are deliberately absent from [`EffectiveHarnessCapabilities`]
//! rather than advertised as booleans that no runtime branch reads.
//!
//! Unknown/missing clients retain the existing CLI fallback. Every Ocean
//! Surface host is mapped explicitly: the browser/PWA (`surface-web`), the
//! Chrome extension (`surface-extension`) and the Tauri desktop shell
//! (`surface-tauri`) all run the one shared Leptos/WASM Surface UI, so they
//! share the Web profile, and Surface voice turns (`leo-voice`) use Voice.
//! Before this mapping the Tauri desktop fell through to the CLI profile
//! (ocean-surface PR #233 found the drift against `session-wire.json`).

/// Which effective harness bundle a turn uses, resolved from `client_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HarnessProfile {
    /// Ocean TUI.
    Tui,
    /// The Ocean Surface hosts: browser/PWA, Chrome extension, Tauri desktop.
    Web,
    /// Voice-only turns.
    Voice,
    /// CLI plus the compatibility fallback for missing/unknown callers.
    Cli,
    /// Agent Client Protocol bridge (`ocean-acp`, currently Zed).
    Acp,
}

/// The complete set of behaviors currently controlled by [`HarnessProfile`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EffectiveHarnessCapabilities {
    /// Tag `read` output, retain snapshots, and offer `hashline_edit`.
    pub hashline_edits: bool,
    /// Spill oversized tool results and expose their `artifact://` recovery URI.
    pub artifact_spill: bool,
    /// Offer the `lsp` code-intelligence tool (TASK-26). Voice turns cannot use
    /// it: definitions, references, and diagnostics are dense structured text
    /// that is unusable when spoken, and the tool's own registration cost is
    /// pointless on a surface that will never render it.
    pub code_intelligence: bool,
}

impl HarnessProfile {
    /// Resolve the current profile without changing compatibility behavior.
    ///
    /// Known mappings:
    /// - `tui` → [`Tui`](Self::Tui)
    /// - `surface-web`, `surface-extension`, `surface-tauri` → [`Web`](Self::Web)
    /// - `leo-voice`, `call-voice` → [`Voice`](Self::Voice)
    /// - `cli` → [`Cli`](Self::Cli)
    /// - `acp-zed` → [`Acp`](Self::Acp)
    ///
    /// Every other value, including empty/missing values, internal `room` and
    /// `heartbeat` turns, and currently unmapped external surfaces, retains the
    /// existing [`Cli`](Self::Cli) fallback.
    pub fn from_client_type(client_type: Option<&str>) -> Self {
        match client_type {
            Some("tui") => Self::Tui,
            Some("surface-web") | Some("surface-extension") | Some("surface-tauri") => Self::Web,
            Some("leo-voice") | Some("call-voice") => Self::Voice,
            Some("cli") => Self::Cli,
            Some("acp-zed") => Self::Acp,
            _ => Self::Cli,
        }
    }

    /// Return the effective gates applied to `PromptControl` for this turn.
    ///
    /// This matrix is behavior-compatible with the pre-reconciliation code:
    /// ACP previously fell through to CLI, and both profiles had these two gates
    /// enabled. Mapping `acp-zed` explicitly therefore corrects attribution
    /// without changing its tool behavior.
    pub fn effective_capabilities(self) -> EffectiveHarnessCapabilities {
        match self {
            Self::Tui | Self::Cli | Self::Acp => EffectiveHarnessCapabilities {
                hashline_edits: true,
                artifact_spill: true,
                code_intelligence: true,
            },
            Self::Web => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: true,
                code_intelligence: true,
            },
            Self::Voice => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
                code_intelligence: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_client_types_map_table_driven() {
        let cases = [
            ("tui", HarnessProfile::Tui),
            ("surface-web", HarnessProfile::Web),
            ("surface-extension", HarnessProfile::Web),
            ("surface-tauri", HarnessProfile::Web),
            ("leo-voice", HarnessProfile::Voice),
            ("call-voice", HarnessProfile::Voice),
            ("cli", HarnessProfile::Cli),
            ("acp-zed", HarnessProfile::Acp),
        ];
        for (client_type, expected) in cases {
            assert_eq!(
                HarnessProfile::from_client_type(Some(client_type)),
                expected,
                "client_type={client_type}"
            );
        }
    }

    #[test]
    fn verified_in_repo_emitters_keep_their_effective_behavior() {
        // Source anchors: ocean-tui/shell, ocean-cli, ocean-acp/daemon,
        // daemon voice adapters + persistent_rooms, and ocean-heartbeat.
        let cases = [
            ("tui", HarnessProfile::Tui, true, true, true),
            ("cli", HarnessProfile::Cli, true, true, true),
            ("acp-zed", HarnessProfile::Acp, true, true, true),
            ("leo-voice", HarnessProfile::Voice, false, false, false),
            ("call-voice", HarnessProfile::Voice, false, false, false),
            ("room", HarnessProfile::Cli, true, true, true),
            ("heartbeat", HarnessProfile::Cli, true, true, true),
        ];
        for (client_type, profile, hashline_edits, artifact_spill, code_intelligence) in cases {
            let resolved = HarnessProfile::from_client_type(Some(client_type));
            assert_eq!(resolved, profile, "client_type={client_type}");
            assert_eq!(
                resolved.effective_capabilities(),
                EffectiveHarnessCapabilities {
                    hashline_edits,
                    artifact_spill,
                    code_intelligence,
                },
                "client_type={client_type}"
            );
        }
    }

    #[test]
    fn surface_tauri_desktop_gets_the_web_profile_not_the_cli_fallback() {
        // The Tauri desktop hosts the same Leptos/WASM Surface as the browser
        // PWA, and `ocean-agent` already gives it the web surface prompt. Its
        // effective gates must equal the web Surface's, not the CLI fallback's
        // (which would turn hashline-tagged reads and `hashline_edit` on).
        let tauri = HarnessProfile::from_client_type(Some("surface-tauri"));
        assert_eq!(tauri, HarnessProfile::Web);
        assert_eq!(
            tauri.effective_capabilities(),
            HarnessProfile::from_client_type(Some("surface-web")).effective_capabilities()
        );
        assert_ne!(
            tauri.effective_capabilities(),
            HarnessProfile::from_client_type(None).effective_capabilities()
        );
    }

    #[test]
    fn unknown_empty_and_missing_clients_retain_cli_fallback() {
        // `surface-gpui` is the abandoned GPUI desktop (ocean-surface froze
        // `crates/ocean-gui` on 2026-07-21); it is not a live host and keeps
        // the fallback rather than being restored as a client identifier.
        for client_type in [
            "surface-gpui",
            "surface-slack",
            "surface-canvas",
            "surface-mobile",
            "heartbeat-cron",
            "unknown",
            "",
        ] {
            assert_eq!(
                HarnessProfile::from_client_type(Some(client_type)),
                HarnessProfile::Cli,
                "client_type={client_type}"
            );
        }
        assert_eq!(HarnessProfile::from_client_type(None), HarnessProfile::Cli);
    }

    #[test]
    fn effective_matrix_contains_only_shipped_profile_gates() {
        let full = EffectiveHarnessCapabilities {
            hashline_edits: true,
            artifact_spill: true,
            code_intelligence: true,
        };
        assert_eq!(HarnessProfile::Tui.effective_capabilities(), full);
        assert_eq!(HarnessProfile::Acp.effective_capabilities(), full);
        assert_eq!(HarnessProfile::Cli.effective_capabilities(), full);
        assert_eq!(
            HarnessProfile::Web.effective_capabilities(),
            EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: true,
                code_intelligence: true,
            }
        );
        assert_eq!(
            HarnessProfile::Voice.effective_capabilities(),
            EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
                code_intelligence: false,
            }
        );
    }
}
