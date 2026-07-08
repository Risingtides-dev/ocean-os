//! W0 — harness-profile seam (OMP port foundation).
//!
//! Ocean's IDE-grade harness (hashline edits, stream rules, rich context
//! collection, artifacts, the context minimizer) is not a single global flag: it
//! is scoped **per surface**. A voice turn and a full TUI turn should not carry
//! the same *presentation* harness weight. The two harness *tools* — LSP code
//! intelligence and long-term memory (`retain`/`recall`) — are the exception:
//! they belong to the harness regardless of surface and every profile carries
//! them (LSP self-gates on server detection). Every turn already arrives with a
//! `client_type` (`"tui"`, `"surface-web"`, `"surface-gpui"`, `"surface-native"`,
//! `"cli"`, `"leo-voice"`, `"surface-extension"`, …), so we collapse that raw
//! string into a small [`HarnessProfile`] enum and expose a
//! [`HarnessCapabilities`] bundle per profile.
//!
//! This is the SEAM, not the behaviour gate. Future OMP-port features (hashline
//! edits, LSP, stream rules, …) attach to a profile in one line — they read
//! `profile.capabilities().hashline_edits` instead of introducing yet another
//! global toggle. W0 only establishes that the profile resolves correctly from
//! `client_type` and that the capability matrix is right; it does not yet gate
//! any turn behaviour.
//
// TODO: config override per docs/specs/2026-07-03-omp-port-map.md — the
// capability bundles below are hardcoded sensible defaults; a later wave lets
// operators override the bundle per profile from config without a code change.

/// Which harness capability bundle a turn runs under, resolved from the turn's
/// `client_type`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HarnessProfile {
    /// Full IDE-grade harness. The TUI is the reference surface: everything on.
    Tui,
    /// Lean conversational surface (web / browser extension). Memory + artifacts
    /// only; none of the IDE machinery.
    Web,
    /// Minimal surface (hands-free voice). Memory only — no editing/IDE weight.
    Voice,
    /// Scripting surface. Reliable edits (hashline) + minimizer + artifacts, but
    /// not the full IDE (no LSP / stream rules / rich context).
    Cli,
    /// Agent-Client-Protocol host. Same backend capabilities as [`Tui`] — an ACP
    /// editor drives its own rendering, so nothing here is TUI-render-specific.
    ///
    /// Not yet constructed by [`from_client_type`]: no `client_type` maps to it
    /// until the ACP host lands (a later OMP-port wave routes ACP turns here).
    /// The variant + its capability bundle exist now so the seam is complete.
    ///
    /// [`Tui`]: HarnessProfile::Tui
    /// [`from_client_type`]: HarnessProfile::from_client_type
    #[allow(dead_code)]
    Acp,
}

/// The set of harness capabilities that are live for a given [`HarnessProfile`].
///
/// Future capability checks read these fields (e.g. `if caps.hashline_edits { … }`)
/// rather than branching on `client_type` or a global flag directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HarnessCapabilities {
    /// Language-server integration (diagnostics, hovers, symbol nav).
    pub lsp: bool,
    /// Hashline-anchored precise edits.
    pub hashline_edits: bool,
    /// Streaming output rules / transforms.
    pub stream_rules: bool,
    /// Rich context collection (workspace scan, open-file context, etc.).
    pub rich_context: bool,
    /// Long-term memory read/write.
    pub memory: bool,
    /// Artifact generation / rendering.
    pub artifacts: bool,
    /// Context minimizer (prompt/context compaction).
    pub minimizer: bool,
}

impl HarnessProfile {
    /// Resolve a [`HarnessProfile`] from a turn's `client_type`.
    ///
    /// Known mappings:
    /// - `"tui"` → [`Tui`]
    /// - `"surface-web"`, `"surface-extension"` → [`Web`]
    /// - `"leo-voice"` → [`Voice`]
    /// - `"cli"` → [`Cli`]
    ///
    /// Anything else (including unrecognised surfaces like `"surface-gpui"` /
    /// `"surface-native"`, transient `client_type`s such as `"call-voice"` /
    /// `"room"`, or a missing `client_type`) falls back to [`Cli`]. `Cli` is the
    /// conservative default: it grants reliable edits + artifacts but withholds
    /// the heavy IDE machinery (LSP, stream rules, rich context), so an unknown
    /// caller never silently inherits the full harness weight — it opts in by
    /// declaring a known `client_type`.
    ///
    /// [`Tui`]: HarnessProfile::Tui
    /// [`Web`]: HarnessProfile::Web
    /// [`Voice`]: HarnessProfile::Voice
    /// [`Cli`]: HarnessProfile::Cli
    pub fn from_client_type(client_type: Option<&str>) -> HarnessProfile {
        match client_type {
            Some("tui") => HarnessProfile::Tui,
            Some("surface-web") | Some("surface-extension") => HarnessProfile::Web,
            Some("leo-voice") => HarnessProfile::Voice,
            Some("cli") => HarnessProfile::Cli,
            _ => HarnessProfile::Cli,
        }
    }

    /// The capability bundle live for this profile.
    ///
    /// Hardcoded sensible defaults (see the module-level TODO for the planned
    /// config override). The matrix:
    ///
    /// `lsp` and `memory` are surface-UNIVERSAL: they are code-intelligence and
    /// long-term-recall *tools* that belong to the harness itself, not to any one
    /// face, so every profile carries them. Only the presentation-shaped
    /// capabilities (hashline edits, stream rules, rich context, minimizer,
    /// artifacts) are scoped per surface. `lsp` still self-gates at the provider:
    /// the tool is only offered when a language server is actually detected.
    ///
    /// | profile | lsp | hashline | stream_rules | rich_context | memory | artifacts | minimizer |
    /// |---------|-----|----------|--------------|--------------|--------|-----------|-----------|
    /// | Tui     |  ✓  |    ✓     |      ✓       |      ✓       |   ✓    |     ✓     |     ✓     |
    /// | Acp     |  ✓  |    ✓     |      ✓       |      ✓       |   ✓    |     ✓     |     ✓     |
    /// | Web     |  ✓  |    ✗     |      ✗       |      ✗       |   ✓    |     ✓     |     ✗     |
    /// | Voice   |  ✓  |    ✗     |      ✗       |      ✗       |   ✓    |     ✗     |     ✗     |
    /// | Cli     |  ✓  |    ✓     |      ✗       |      ✗       |   ✓    |     ✓     |     ✓     |
    pub fn capabilities(&self) -> HarnessCapabilities {
        match self {
            // Full IDE-grade harness. ACP matches Tui for backend capabilities:
            // an ACP host renders its own UI, so nothing here is TUI-specific.
            HarnessProfile::Tui | HarnessProfile::Acp => HarnessCapabilities {
                lsp: true,
                hashline_edits: true,
                stream_rules: true,
                rich_context: true,
                memory: true,
                artifacts: true,
                minimizer: true,
            },
            // Lean conversational surface: universal tools (lsp + memory), plus
            // artifacts; drop the presentation-heavy IDE machinery.
            HarnessProfile::Web => HarnessCapabilities {
                lsp: true,
                hashline_edits: false,
                stream_rules: false,
                rich_context: false,
                memory: true,
                artifacts: true,
                minimizer: false,
            },
            // Minimal hands-free surface: universal tools only (lsp + memory).
            HarnessProfile::Voice => HarnessCapabilities {
                lsp: true,
                hashline_edits: false,
                stream_rules: false,
                rich_context: false,
                memory: true,
                artifacts: false,
                minimizer: false,
            },
            // Scripting surface: universal tools + reliable edits + minimizer +
            // artifacts, no presentation IDE.
            HarnessProfile::Cli => HarnessCapabilities {
                lsp: true,
                hashline_edits: true,
                stream_rules: false,
                rich_context: false,
                memory: true,
                artifacts: true,
                minimizer: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_client_type_maps_every_known_surface() {
        assert_eq!(
            HarnessProfile::from_client_type(Some("tui")),
            HarnessProfile::Tui
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("surface-web")),
            HarnessProfile::Web
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("surface-extension")),
            HarnessProfile::Web
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("leo-voice")),
            HarnessProfile::Voice
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("cli")),
            HarnessProfile::Cli
        );
    }

    #[test]
    fn from_client_type_falls_back_to_cli_for_unknown_and_none() {
        // Unrecognised surfaces and transient client_types default to Cli.
        assert_eq!(
            HarnessProfile::from_client_type(Some("surface-gpui")),
            HarnessProfile::Cli
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("surface-native")),
            HarnessProfile::Cli
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("call-voice")),
            HarnessProfile::Cli
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("room")),
            HarnessProfile::Cli
        );
        assert_eq!(
            HarnessProfile::from_client_type(Some("")),
            HarnessProfile::Cli
        );
        // Missing client_type is the conservative default too.
        assert_eq!(HarnessProfile::from_client_type(None), HarnessProfile::Cli);
    }

    #[test]
    fn tui_profile_lights_up_the_full_harness() {
        let caps = HarnessProfile::Tui.capabilities();
        assert!(caps.lsp);
        assert!(caps.hashline_edits);
        assert!(caps.stream_rules);
        assert!(caps.rich_context);
        assert!(caps.memory);
        assert!(caps.artifacts);
        assert!(caps.minimizer);
    }

    #[test]
    fn acp_matches_tui_backend_capabilities() {
        assert_eq!(
            HarnessProfile::Acp.capabilities(),
            HarnessProfile::Tui.capabilities()
        );
    }

    #[test]
    fn web_profile_is_lean_conversational() {
        let caps = HarnessProfile::Web.capabilities();
        // Universal harness tools (lsp + memory) + artifacts stay on…
        assert!(caps.lsp);
        assert!(caps.memory);
        assert!(caps.artifacts);
        // …the presentation IDE machinery is off.
        assert!(!caps.hashline_edits);
        assert!(!caps.stream_rules);
        assert!(!caps.rich_context);
        assert!(!caps.minimizer);
    }

    #[test]
    fn voice_profile_carries_universal_tools_only() {
        let caps = HarnessProfile::Voice.capabilities();
        // Universal harness tools reach even the leanest surface.
        assert!(caps.memory);
        assert!(caps.lsp);
        // Everything presentation-shaped is off.
        assert!(!caps.hashline_edits);
        assert!(!caps.stream_rules);
        assert!(!caps.rich_context);
        assert!(!caps.artifacts);
        assert!(!caps.minimizer);
    }

    #[test]
    fn cli_profile_is_a_reliable_scripting_surface() {
        let caps = HarnessProfile::Cli.capabilities();
        // Reliable edits + minimizer + artifacts on, plus the universal tools…
        assert!(caps.hashline_edits);
        assert!(caps.minimizer);
        assert!(caps.artifacts);
        assert!(caps.lsp);
        assert!(caps.memory);
        // …no presentation IDE machinery.
        assert!(!caps.stream_rules);
        assert!(!caps.rich_context);
    }
}
