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
//! Unknown/missing clients retain the existing CLI fallback. This module does
//! not decide new policy for externally owned surface tags such as
//! `surface-tauri`; it records and tests the current behavior until that
//! cross-repository mapping is approved separately.

/// Which effective harness bundle a turn uses, resolved from `client_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HarnessProfile {
    /// Ocean TUI.
    Tui,
    /// Browser/PWA and browser-extension surfaces.
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
}

impl HarnessProfile {
    /// Resolve the current profile without changing compatibility behavior.
    ///
    /// Known mappings:
    /// - `tui` → [`Tui`](Self::Tui)
    /// - `surface-web`, `surface-extension` → [`Web`](Self::Web)
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
            Some("surface-web") | Some("surface-extension") => Self::Web,
            Some("leo-voice") | Some("call-voice") => Self::Voice,
            Some("cli") => Self::Cli,
            Some("acp-zed") => Self::Acp,
            _ => Self::Cli,
        }
    }

    /// Return the two effective gates applied to `PromptControl` for this turn.
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
            },
            Self::Web => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: true,
            },
            Self::Voice => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
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
            ("tui", HarnessProfile::Tui, true, true),
            ("cli", HarnessProfile::Cli, true, true),
            ("acp-zed", HarnessProfile::Acp, true, true),
            ("leo-voice", HarnessProfile::Voice, false, false),
            ("call-voice", HarnessProfile::Voice, false, false),
            ("room", HarnessProfile::Cli, true, true),
            ("heartbeat", HarnessProfile::Cli, true, true),
        ];
        for (client_type, profile, hashline_edits, artifact_spill) in cases {
            let resolved = HarnessProfile::from_client_type(Some(client_type));
            assert_eq!(resolved, profile, "client_type={client_type}");
            assert_eq!(
                resolved.effective_capabilities(),
                EffectiveHarnessCapabilities {
                    hashline_edits,
                    artifact_spill,
                },
                "client_type={client_type}"
            );
        }
    }

    #[test]
    fn unknown_empty_and_missing_clients_retain_cli_fallback() {
        // `surface-tauri` is the cross-repo mismatch recorded in
        // docs/OCEAN_PROJECT_MAP.md. This checkpoint documents rather than
        // silently reclassifies it.
        for client_type in [
            "surface-tauri",
            "surface-gpui",
            "surface-native",
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
        };
        assert_eq!(HarnessProfile::Tui.effective_capabilities(), full);
        assert_eq!(HarnessProfile::Acp.effective_capabilities(), full);
        assert_eq!(HarnessProfile::Cli.effective_capabilities(), full);
        assert_eq!(
            HarnessProfile::Web.effective_capabilities(),
            EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: true,
            }
        );
        assert_eq!(
            HarnessProfile::Voice.effective_capabilities(),
            EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
            }
        );
    }
}
