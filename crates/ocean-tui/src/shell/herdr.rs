//! Best-effort lifecycle reporting to a surrounding Herdr pane.
//!
//! Herdr injects `HERDR_ENV`, `HERDR_PANE_ID`, and `HERDR_BIN_PATH` into
//! managed panes. When present, Ocean projects its existing authoritative TUI
//! lifecycle and bound session identity onto Herdr's socket-backed CLI so a
//! Herdr server restart can resume with `ocean --session <id>`. Reporting is
//! deliberately fail-soft and runs off-thread so Herdr can never block the Elm
//! loop.

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(not(test))]
use std::time::{SystemTime, UNIX_EPOCH};

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent};

use super::action::Action;

/// Official Herdr source label. Must match a planner entry in Herdr's
/// `agent_resume` table (`herdr:ocean` / `ocean`) for native restore.
const SOURCE: &str = "herdr:ocean";
const AGENT: &str = "ocean";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    herdr_bin: OsString,
    pane_id: String,
}

/// Projects Ocean's TUI lifecycle into Herdr when Ocean is running in a Herdr
/// pane. A disabled reporter still tracks transitions in tests, but performs no
/// process I/O.
#[derive(Debug, Default)]
pub struct Reporter {
    config: Option<Config>,
    state: Option<State>,
    turn_active: bool,
    pending_permissions: usize,
    /// Last session id successfully projected to Herdr.
    reported_session: Option<String>,
    /// True after any session has been bound in this process. Distinguishes the
    /// first mint (`startup`) from later `/new` mints (`new`).
    ever_bound: bool,
    seq: u64,
}

impl Reporter {
    pub fn from_env() -> Self {
        // Unit tests run inside the operator's Herdr pane too. They must never
        // take lifecycle authority for the pane that launched `cargo test`.
        #[cfg(test)]
        {
            Self::default()
        }

        #[cfg(not(test))]
        {
            let enabled = std::env::var("HERDR_ENV").ok().as_deref() == Some("1");
            let pane_id = std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|value| !value.trim().is_empty());
            let config = enabled.then_some(()).and_then(|_| {
                pane_id.map(|pane_id| Config {
                    herdr_bin: std::env::var_os("HERDR_BIN_PATH")
                        .unwrap_or_else(|| OsString::from("herdr")),
                    pane_id,
                })
            });
            let mut reporter = Self::default();
            reporter.config = config;
            reporter.seq = initial_seq();
            reporter.set_state(State::Idle);
            reporter
        }
    }

    /// Observe an action only after [`super::app::App::dispatch`] has rejected
    /// stale-session agent events and applied its authoritative state changes.
    pub fn observe(&mut self, action: &Action, bound_session: Option<AgentSessionId>) {
        let session_start_source = match action {
            Action::ResumeSession { .. } => Some("resume"),
            Action::SessionBound(_) => Some(if self.ever_bound { "new" } else { "startup" }),
            Action::NewSession | Action::NewSessionInProject { .. } => None,
            _ => None,
        };
        self.sync_session(bound_session, session_start_source);

        match action {
            Action::SubmitPrompt(_) => {
                self.turn_active = true;
                self.pending_permissions = 0;
                self.set_state(State::Working);
            }
            Action::AgentEvent(event)
                if event.session_id().is_some() && event.session_id() == bound_session =>
            {
                match event.as_ref() {
                    AgentTurnEvent::TurnStarted { .. } => {
                        self.turn_active = true;
                        self.pending_permissions = 0;
                        self.set_state(State::Working);
                    }
                    AgentTurnEvent::TurnFinished { .. } => self.finish_turn(),
                    _ => {}
                }
            }
            Action::OceanEvent(envelope) if envelope_matches_session(envelope, bound_session) => {
                match &envelope.event {
                    ocean_core::OceanEvent::PermissionRequest { .. } => {
                        self.pending_permissions = self.pending_permissions.saturating_add(1);
                        self.set_state(State::Blocked);
                    }
                    ocean_core::OceanEvent::PermissionDecision { .. } => {
                        self.pending_permissions = self.pending_permissions.saturating_sub(1);
                        if self.pending_permissions == 0 {
                            self.set_state(if self.turn_active {
                                State::Working
                            } else {
                                State::Idle
                            });
                        }
                    }
                    _ => {}
                }
            }
            Action::TurnSendFailed { .. } | Action::TurnOutcomeUnknown { .. } => self.finish_turn(),
            Action::NewSession
            | Action::NewSessionInProject { .. }
            | Action::ResumeSession { .. } => self.finish_turn(),
            _ => {}
        }
    }

    pub fn release(&mut self) {
        let Some(config) = self.config.take() else {
            return;
        };
        self.reported_session = None;
        self.seq = self.seq.saturating_add(1);
        run_bounded(
            config.herdr_bin,
            vec![
                "pane".into(),
                "release-agent".into(),
                config.pane_id,
                "--source".into(),
                SOURCE.into(),
                "--agent".into(),
                AGENT.into(),
                "--seq".into(),
                self.seq.to_string(),
            ],
            herdr_release_timeout(),
        );
    }

    fn sync_session(
        &mut self,
        bound_session: Option<AgentSessionId>,
        session_start_source: Option<&'static str>,
    ) {
        let next = bound_session.map(|id| id.0.to_string());
        if self.reported_session == next {
            return;
        }
        self.reported_session = next.clone();
        if next.is_some() {
            self.ever_bound = true;
        }

        let Some(config) = self.config.clone() else {
            return;
        };
        let Some(session_id) = next else {
            // Unbound `/new` leaves no durable Ocean session. Herdr has no
            // clear-without-replacement API, so the previous official reference
            // remains until the next bind or pane release.
            return;
        };

        self.seq = self.seq.saturating_add(1);
        let mut args = vec![
            "pane".into(),
            "report-agent-session".into(),
            config.pane_id,
            "--source".into(),
            SOURCE.into(),
            "--agent".into(),
            AGENT.into(),
            "--agent-session-id".into(),
            session_id,
            "--seq".into(),
            self.seq.to_string(),
        ];
        if let Some(source) = session_start_source {
            args.push("--session-start-source".into());
            args.push(source.into());
        }
        launch(config.herdr_bin, args);
    }

    fn finish_turn(&mut self) {
        self.turn_active = false;
        self.pending_permissions = 0;
        self.set_state(State::Idle);
    }

    fn set_state(&mut self, state: State) {
        if self.state == Some(state) {
            return;
        }
        self.state = Some(state);
        let Some(config) = self.config.clone() else {
            return;
        };
        self.seq = self.seq.saturating_add(1);
        let mut args = vec![
            "pane".into(),
            "report-agent".into(),
            config.pane_id,
            "--source".into(),
            SOURCE.into(),
            "--agent".into(),
            AGENT.into(),
            "--state".into(),
            state.as_str().into(),
            "--seq".into(),
            self.seq.to_string(),
        ];
        if let Some(session_id) = self.reported_session.clone() {
            args.push("--agent-session-id".into());
            args.push(session_id);
        }
        launch(config.herdr_bin, args);
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
impl Reporter {
    pub fn reported_session(&self) -> Option<&str> {
        self.reported_session.as_deref()
    }
    pub fn is_ever_bound(&self) -> bool {
        self.ever_bound
    }
}

fn envelope_matches_session(
    envelope: &ocean_core::EventEnvelope,
    bound_session: Option<AgentSessionId>,
) -> bool {
    matches!(
        (envelope.session_id, bound_session),
        (Some(event_session), Some(bound)) if event_session == bound.0
    )
}

#[cfg(not(test))]
fn initial_seq() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| {
            duration
                .as_millis()
                .saturating_mul(1_000)
                .min(u128::from(u64::MAX)) as u64
        })
        .unwrap_or(0)
}

fn launch(program: OsString, args: Vec<String>) {
    // A short-lived reaper thread keeps state updates off the async/UI path
    // without leaving zombies. stdout/stderr stay detached from ratatui.
    let _ = std::thread::Builder::new()
        .name("ocean-herdr-report".into())
        .spawn(move || {
            let _ = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
}

#[cfg(not(test))]
fn herdr_release_timeout() -> Duration {
    Duration::from_millis(300)
}

#[cfg(test)]
fn herdr_release_timeout() -> Duration {
    // Workspace tests can heavily contend for CPU/process slots. Preserve the
    // production shutdown cap while giving the fixture enough time to prove
    // that release waits for its command to finish.
    Duration::from_secs(3)
}

/// Deliver lifecycle release before the TUI process exits, but cap the shutdown
/// cost so a missing or wedged Herdr binary cannot trap the terminal.
fn run_bounded(program: OsString, args: Vec<String>, timeout: Duration) {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ocean_agent_sdk::{AgentTurnId, AgentTurnStatus};
    use ocean_core::{EventEnvelope, OceanEvent};
    use uuid::Uuid;

    use super::*;

    fn session(value: u128) -> AgentSessionId {
        AgentSessionId(Uuid::from_u128(value))
    }

    fn started(sid: AgentSessionId) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: sid,
            turn_id: AgentTurnId(Uuid::from_u128(10)),
            model: None,
        }))
    }

    fn finished(sid: AgentSessionId) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnFinished {
            session_id: sid,
            turn_id: AgentTurnId(Uuid::from_u128(10)),
            status: AgentTurnStatus::Completed,
            error: None,
            wall_ms: None,
            output_tokens: None,
            input_tokens: None,
            cache_read_tokens: None,
            tokens_per_second: None,
            context_usage: None,
        }))
    }

    fn permission(sid: AgentSessionId, event: OceanEvent) -> Action {
        Action::OceanEvent(Box::new(EventEnvelope {
            id: Uuid::from_u128(20),
            at: Utc::now(),
            session_id: Some(sid.0),
            request_id: Some(Uuid::from_u128(21)),
            permission_id: Some(Uuid::from_u128(22)),
            origin: None,
            event,
        }))
    }

    #[test]
    fn lifecycle_tracks_turn_permission_and_finish() {
        let sid = session(1);
        let mut reporter = Reporter::default();

        reporter.observe(&started(sid), Some(sid));
        assert_eq!(reporter.state, Some(State::Working));
        assert!(reporter.turn_active);

        reporter.observe(
            &permission(
                sid,
                OceanEvent::PermissionRequest {
                    tool: "bash".into(),
                    reason: "mutating".into(),
                    args: serde_json::json!({}),
                },
            ),
            Some(sid),
        );
        assert_eq!(reporter.state, Some(State::Blocked));
        assert_eq!(reporter.pending_permissions, 1);

        reporter.observe(
            &permission(
                sid,
                OceanEvent::PermissionDecision {
                    allowed: true,
                    reason: None,
                },
            ),
            Some(sid),
        );
        assert_eq!(reporter.state, Some(State::Working));
        assert_eq!(reporter.pending_permissions, 0);

        reporter.observe(&finished(sid), Some(sid));
        assert_eq!(reporter.state, Some(State::Idle));
        assert!(!reporter.turn_active);
    }

    #[test]
    fn permission_for_another_session_does_not_take_authority() {
        let sid = session(1);
        let other = session(2);
        let mut reporter = Reporter::default();
        reporter.observe(&started(sid), Some(sid));

        reporter.observe(
            &permission(
                other,
                OceanEvent::PermissionRequest {
                    tool: "bash".into(),
                    reason: "mutating".into(),
                    args: serde_json::json!({}),
                },
            ),
            Some(sid),
        );

        assert_eq!(reporter.state, Some(State::Working));
        assert_eq!(reporter.pending_permissions, 0);
    }

    #[test]
    fn agent_event_requires_the_current_bound_session() {
        let sid = session(1);
        let other = session(2);
        let mut reporter = Reporter::default();

        reporter.observe(&started(other), Some(sid));
        assert_eq!(reporter.state, None);
        assert!(!reporter.turn_active);

        reporter.observe(&started(sid), None);
        assert_eq!(reporter.state, None);
        assert!(!reporter.turn_active);
    }

    #[test]
    fn send_failures_return_to_idle() {
        let mut reporter = Reporter::default();
        reporter.observe(&Action::SubmitPrompt("hello".into()), None);
        assert_eq!(reporter.state, Some(State::Working));

        reporter.observe(
            &Action::TurnSendFailed {
                prompt: "hello".into(),
                err: "offline".into(),
            },
            None,
        );
        assert_eq!(reporter.state, Some(State::Idle));
        assert!(!reporter.turn_active);
    }

    #[test]
    fn tracks_bound_session_identity() {
        let sid = session(7);
        let mut reporter = Reporter::default();

        reporter.observe(&Action::SessionBound(sid), Some(sid));
        assert_eq!(
            reporter.reported_session.as_deref(),
            Some(sid.0.to_string().as_str())
        );
        assert!(reporter.ever_bound);

        reporter.observe(&Action::NewSession, None);
        // Locally we drop the bound id; Herdr keeps the last official ref until
        // the next bind/release because it has no clear-without-replacement API.
        assert_eq!(reporter.reported_session, None);
        assert!(reporter.ever_bound);
    }

    #[cfg(unix)]
    #[test]
    fn release_waits_for_the_report_command() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ocean-herdr-release-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let script = dir.join("fake-herdr.sh");
        let marker = dir.join("args.txt");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                marker.display()
            ),
        )
        .expect("write fake herdr");
        let mut permissions = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("make script executable");

        let mut reporter = Reporter::default();
        reporter.config = Some(Config {
            herdr_bin: script.into_os_string(),
            pane_id: "w1:p9".into(),
        });
        reporter.reported_session = Some("sess".into());
        reporter.release();

        let args = fs::read_to_string(&marker).expect("release command completed");
        assert!(args.contains("release-agent"));
        assert!(args.contains("w1:p9"));
        assert!(args.contains(SOURCE));
        assert!(args.contains(AGENT));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn session_bind_reports_agent_session_id() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ocean-herdr-session-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let script = dir.join("fake-herdr.sh");
        let marker = dir.join("args.txt");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                marker.display()
            ),
        )
        .expect("write fake herdr");
        let mut permissions = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("make script executable");

        let sid = session(42);
        let mut reporter = Reporter::default();
        reporter.config = Some(Config {
            herdr_bin: script.into_os_string(),
            pane_id: "w1:p3".into(),
        });
        reporter.observe(&Action::SessionBound(sid), Some(sid));
        // Give the detached reporter thread a moment to finish.
        std::thread::sleep(Duration::from_millis(50));

        let args = wait_for_marker(&marker);
        assert!(args.contains("report-agent-session"));
        assert!(args.contains("--agent-session-id"));
        assert!(args.contains(&sid.0.to_string()));
        assert!(args.contains("startup") || args.contains("new"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn resume_session_reports_agent_session_id_with_resume_source() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ocean-herdr-resume-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let script = dir.join("fake-herdr.sh");
        let marker = dir.join("args.txt");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                marker.display()
            ),
        )
        .expect("write fake herdr");
        let mut permissions = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).expect("make script executable");

        let sid = session(77);
        let mut reporter = Reporter::default();
        reporter.config = Some(Config {
            herdr_bin: script.into_os_string(),
            pane_id: "w1:p4".into(),
        });
        reporter.observe(
            &Action::ResumeSession {
                id: sid,
                path: std::path::PathBuf::from("/tmp/fake"),
                cwd: std::path::PathBuf::from("/tmp/fake"),
            },
            Some(sid),
        );
        std::thread::sleep(Duration::from_millis(50));

        let args = wait_for_marker(&marker);
        assert!(args.contains("report-agent-session"));
        assert!(args.contains("--agent-session-id"));
        assert!(args.contains(&sid.0.to_string()));
        assert!(
            args.contains("--session-start-source"),
            "ResumeSession must carry --session-start-source resume"
        );
        assert!(args.contains("resume"));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    fn wait_for_marker(path: &std::path::Path) -> String {
        for _ in 0..50 {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if !contents.trim().is_empty() {
                    return contents;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(path).expect("marker written")
    }
}
