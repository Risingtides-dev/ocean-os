//! W0 — effective per-turn harness-profile seam.
//!
//! A turn's `client_type` selects only the behaviors this seam actually
//! controls today:
//!
//! - hashline-tagged reads plus `hashline_edit`;
//! - oversized tool-result spill to `artifact://`;
//! - command-output minimization for explicitly tokenized Bash argv
//!   (minimizer M2c): the runtime still requires the artifact capability and
//!   a bound session store before any minimization can happen.
//!
//! LSP and memory providers are registered globally and are not profile-gated.
//! Stream rules and rich-context collection are not wired, so they are
//! deliberately absent from [`EffectiveHarnessCapabilities`] rather than
//! advertised as booleans that no runtime branch reads.
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
    /// Offer the `lsp` code-intelligence tool (TASK-26). Voice turns cannot use
    /// it: definitions, references, and diagnostics are dense structured text
    /// that is unusable when spoken, and the tool's own registration cost is
    /// pointless on a surface that will never render it.
    pub code_intelligence: bool,
    /// Minimize eligible explicitly tokenized Bash argv output for the active
    /// provider request only, pinning an exact recovery artifact for the run
    /// (minimizer M2c). Mirrors `artifact_spill`: minimization can never
    /// produce an unrecoverable rewrite, and voice turns never run it.
    pub command_output_minimization: bool,
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

    /// Return the effective gates applied to `PromptControl` for this turn.
    ///
    /// This matrix is behavior-compatible with the pre-reconciliation code:
    /// ACP previously fell through to CLI, and both profiles had these gates
    /// enabled. Mapping `acp-zed` explicitly therefore corrects attribution
    /// without changing its tool behavior.
    ///
    /// Command-output minimization (M2c) mirrors artifact spill exactly
    /// (design §3.8): every surface that can spill can minimize, voice does
    /// neither. The runtime additionally requires a bound session store, so
    /// the gate alone can never strand a rewrite.
    pub fn effective_capabilities(self) -> EffectiveHarnessCapabilities {
        match self {
            Self::Tui | Self::Cli | Self::Acp => EffectiveHarnessCapabilities {
                hashline_edits: true,
                artifact_spill: true,
                code_intelligence: true,
                command_output_minimization: true,
            },
            Self::Web => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: true,
                code_intelligence: true,
                command_output_minimization: true,
            },
            Self::Voice => EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
                code_intelligence: false,
                command_output_minimization: false,
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
            ("tui", HarnessProfile::Tui, true, true, true, true),
            ("cli", HarnessProfile::Cli, true, true, true, true),
            ("acp-zed", HarnessProfile::Acp, true, true, true, true),
            (
                "leo-voice",
                HarnessProfile::Voice,
                false,
                false,
                false,
                false,
            ),
            (
                "call-voice",
                HarnessProfile::Voice,
                false,
                false,
                false,
                false,
            ),
            ("room", HarnessProfile::Cli, true, true, true, true),
            ("heartbeat", HarnessProfile::Cli, true, true, true, true),
        ];
        for (
            client_type,
            profile,
            hashline_edits,
            artifact_spill,
            code_intelligence,
            command_output_minimization,
        ) in cases
        {
            let resolved = HarnessProfile::from_client_type(Some(client_type));
            assert_eq!(resolved, profile, "client_type={client_type}");
            assert_eq!(
                resolved.effective_capabilities(),
                EffectiveHarnessCapabilities {
                    hashline_edits,
                    artifact_spill,
                    code_intelligence,
                    command_output_minimization,
                },
                "client_type={client_type}"
            );
        }
    }

    #[test]
    fn compatibility_fallback_rows_enable_command_output_minimization() {
        // Design §3.8: the fallback row is an explicit policy decision —
        // room/heartbeat/missing/unknown/unmapped turns already receive the
        // CLI artifact capability, and M2c gives them the same recoverable
        // model-context reduction. Each tag is named individually.
        let expected = EffectiveHarnessCapabilities {
            hashline_edits: true,
            artifact_spill: true,
            code_intelligence: true,
            command_output_minimization: true,
        };
        // Internal emitters routed through the fallback.
        assert_eq!(
            HarnessProfile::from_client_type(Some("room")).effective_capabilities(),
            expected,
            "room"
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("heartbeat")).effective_capabilities(),
            expected,
            "heartbeat"
        );
        // Missing client_type.
        assert_eq!(
            HarnessProfile::from_client_type(None).effective_capabilities(),
            expected,
            "missing"
        );
        // Unknown tag.
        assert_eq!(
            HarnessProfile::from_client_type(Some("unknown")).effective_capabilities(),
            expected,
            "unknown"
        );
        // Unmapped external surfaces (surface-tauri stays the recorded
        // cross-repo mismatch, not a new profile).
        for unmapped in [
            "surface-tauri",
            "surface-slack",
            "surface-canvas",
            "surface-mobile",
            "",
        ] {
            assert_eq!(
                HarnessProfile::from_client_type(Some(unmapped)).effective_capabilities(),
                expected,
                "unmapped {unmapped:?}"
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
            command_output_minimization: true,
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
                command_output_minimization: true,
            }
        );
        assert_eq!(
            HarnessProfile::Voice.effective_capabilities(),
            EffectiveHarnessCapabilities {
                hashline_edits: false,
                artifact_spill: false,
                code_intelligence: false,
                command_output_minimization: false,
            }
        );
    }
}
