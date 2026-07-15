//! Bounded, fail-open post-turn advisor execution.

use std::{future::Future, sync::Arc, time::Duration};

use ocean_agent_sdk::AgentTurnId;
use serde_json::{json, Value};

use crate::metrics::{AdvisorInFlightGuard, AdvisorOutcome, TurnMetrics};

pub(super) const ADVISOR_CONCURRENCY_LIMIT: usize = 2;
pub(super) const ADVISOR_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) type AdvisorLimiter = Arc<tokio::sync::Semaphore>;

pub(super) struct AdvisorInput {
    pub(super) timeout: Duration,
    pub(super) turn_id: AgentTurnId,
    pub(super) advisor_alias: String,
    pub(super) operator_prompt: String,
    pub(super) assistant_response: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct AdvisorEmission {
    pub(super) turn_id: AgentTurnId,
    pub(super) note: String,
    pub(super) severity: &'static str,
    pub(super) model: String,
}

impl AdvisorEmission {
    /// Preserve the existing rendering payload and add authoritative turn attribution.
    pub(super) fn payload(&self) -> Value {
        json!({
            "note": self.note,
            "severity": self.severity,
            "model": self.model,
            "turn_id": self.turn_id,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AdvisorExecution {
    Emitted(AdvisorEmission),
    Suppressed,
    ProviderError,
    Timeout,
    Saturated,
}

/// The advisor's tight system instruction. It watches, it does not chat: a real
/// concern in 1-2 sentences, or exactly nothing.
fn advisor_system_prompt() -> &'static str {
    "You are an advisor silently watching another coding agent. Review the \
     exchange below. If you see a real correctness concern, risk, or blocker, \
     state it in 1-2 sentences. If nothing is wrong, reply with exactly the \
     empty string / NOTHING."
}

/// Build the advisor's user turn from the completed main turn. This content is
/// sent only to the provider and is never logged.
fn advisor_user_prompt(operator_prompt: &str, assistant_response: &str) -> String {
    format!(
        "OPERATOR PROMPT:\n{operator_prompt}\n\nASSISTANT RESPONSE:\n{assistant_response}\n\n\
         Now give your advisor note (1-2 sentences), or NOTHING."
    )
}

fn advisor_note_if_actionable(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sentinel = trimmed
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_lowercase();
    if sentinel.is_empty() || sentinel == "nothing" || sentinel == "none" {
        return None;
    }
    Some(trimmed.to_string())
}

fn advisor_severity(note: &str) -> &'static str {
    let lower = note.to_ascii_lowercase();
    const BLOCKER: &[&str] = &[
        "must not",
        "will break",
        "data loss",
        "will fail",
        "security vulnerability",
        "critical",
        "corrupt",
        "irreversible",
    ];
    const MILD: &[&str] = &[
        "minor",
        "nitpick",
        "consider",
        "might want",
        "optional",
        "cosmetic",
    ];
    if BLOCKER.iter().any(|w| lower.contains(w)) {
        "blocker"
    } else if MILD.iter().any(|w| lower.contains(w)) {
        "info"
    } else {
        "concern"
    }
}

/// Run one post-turn advisor attempt. The dedicated permit is acquired without
/// waiting and held across the provider future. Every non-emission outcome is
/// terminal and fail-open with respect to the already-completed main turn.
pub(super) async fn execute_advisor<F, Fut, E>(
    limiter: AdvisorLimiter,
    metrics: Arc<TurnMetrics>,
    input: AdvisorInput,
    complete: F,
) -> AdvisorExecution
where
    F: FnOnce(String, String, String) -> Fut,
    Fut: Future<Output = Result<(String, String), E>>,
{
    let AdvisorInput {
        timeout,
        turn_id,
        advisor_alias,
        operator_prompt,
        assistant_response,
    } = input;
    let started = std::time::Instant::now();
    let _permit = match limiter.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            metrics.record_advisor(AdvisorOutcome::Saturated, started.elapsed());
            tracing::info!(turn_id = %turn_id, "advisor observer saturated; dropping");
            return AdvisorExecution::Saturated;
        }
    };

    let in_flight = AdvisorInFlightGuard::enter(metrics.clone());
    tracing::info!(turn_id = %turn_id, model = %advisor_alias, "advisor observer started");
    let user_prompt = advisor_user_prompt(&operator_prompt, &assistant_response);
    let completion = complete(
        advisor_alias,
        advisor_system_prompt().to_string(),
        user_prompt,
    );
    let result = tokio::time::timeout(timeout, completion).await;
    drop(in_flight);

    match result {
        Err(_) => {
            metrics.record_advisor(AdvisorOutcome::Timeout, started.elapsed());
            tracing::warn!(turn_id = %turn_id, "advisor observer timed out; dropping");
            AdvisorExecution::Timeout
        }
        Ok(Err(_)) => {
            metrics.record_advisor(AdvisorOutcome::ProviderError, started.elapsed());
            // Provider errors may embed response fragments; record only the
            // fixed outcome and turn attribution, never the error body.
            tracing::warn!(turn_id = %turn_id, "advisor observer provider error; dropping");
            AdvisorExecution::ProviderError
        }
        Ok(Ok((note, model))) => match advisor_note_if_actionable(&note) {
            None => {
                metrics.record_advisor(AdvisorOutcome::Suppressed, started.elapsed());
                tracing::info!(turn_id = %turn_id, model = %model, "advisor observer suppressed");
                AdvisorExecution::Suppressed
            }
            Some(note) => {
                let severity = advisor_severity(&note);
                metrics.record_advisor(AdvisorOutcome::Emitted, started.elapsed());
                tracing::info!(
                    turn_id = %turn_id,
                    severity,
                    model = %model,
                    "advisor observer note emitted"
                );
                AdvisorExecution::Emitted(AdvisorEmission {
                    turn_id,
                    note,
                    severity,
                    model,
                })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::metrics::{labelled_value, metric_value};

    fn limiter(cap: usize) -> AdvisorLimiter {
        Arc::new(tokio::sync::Semaphore::new(cap))
    }

    fn input(timeout: Duration) -> AdvisorInput {
        AdvisorInput {
            timeout,
            turn_id: AgentTurnId::new_v4(),
            advisor_alias: "advisor".into(),
            operator_prompt: "operator".into(),
            assistant_response: "assistant".into(),
        }
    }

    #[test]
    fn production_bounds_are_fixed() {
        assert_eq!(ADVISOR_CONCURRENCY_LIMIT, 2);
        assert_eq!(ADVISOR_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn note_normalization_and_severity_preserve_rendering_behavior() {
        for suppressed in [
            "",
            "   \n  ",
            "NOTHING",
            "nothing",
            "  NOTHING.  ",
            "\"NOTHING\"",
            "None",
        ] {
            assert_eq!(advisor_note_if_actionable(suppressed), None);
        }
        assert_eq!(
            advisor_note_if_actionable("  The retry loop never breaks on cancel.  "),
            Some("The retry loop never breaks on cancel.".to_string())
        );
        assert_eq!(
            advisor_severity("This will break and cause data loss."),
            "blocker"
        );
        assert_eq!(advisor_severity("Minor nitpick: rename this."), "info");
        assert_eq!(advisor_severity("The error path hides failure."), "concern");

        let prompt = advisor_user_prompt("do X", "I did Y");
        assert!(prompt.contains("do X"));
        assert!(prompt.contains("I did Y"));
        assert!(prompt.contains("OPERATOR PROMPT"));
        assert!(prompt.contains("ASSISTANT RESPONSE"));
    }

    #[tokio::test]
    async fn emitted_payload_attributes_originating_turn() {
        let turn_id = AgentTurnId::new_v4();
        let metrics = Arc::new(TurnMetrics::default());
        let mut input = input(Duration::from_secs(1));
        input.turn_id = turn_id;
        input.advisor_alias = "advisor-alias".into();
        let result = execute_advisor(limiter(2), metrics.clone(), input, |_, _, _| async {
            Ok::<_, &'static str>(("A real concern".into(), "resolved-model".into()))
        })
        .await;

        let AdvisorExecution::Emitted(emission) = result else {
            panic!("expected emitted advisor note");
        };
        assert_eq!(emission.turn_id, turn_id);
        assert_eq!(emission.payload()["turn_id"], json!(turn_id));
        assert_eq!(emission.payload()["note"], "A real concern");
        let body = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            labelled_value(&body, "ocean_advisor_outcomes_total{outcome=\"emitted\"}"),
            Some(1)
        );
        assert_eq!(metric_value(&body, "ocean_advisor_in_flight"), Some(0));
    }

    #[tokio::test]
    async fn timeout_is_accounted_and_releases_permit() {
        let limiter = limiter(1);
        let metrics = Arc::new(TurnMetrics::default());
        let result = execute_advisor(
            limiter.clone(),
            metrics.clone(),
            input(Duration::from_millis(10)),
            |_, _, _| std::future::pending::<Result<(String, String), &'static str>>(),
        )
        .await;

        assert_eq!(result, AdvisorExecution::Timeout);
        assert_eq!(limiter.available_permits(), 1);
        let body = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            labelled_value(&body, "ocean_advisor_outcomes_total{outcome=\"timeout\"}"),
            Some(1)
        );
        assert_eq!(metric_value(&body, "ocean_advisor_in_flight"), Some(0));
    }

    #[tokio::test]
    async fn saturation_does_not_invoke_provider_and_running_call_releases_permit() {
        let limiter = limiter(1);
        let metrics = Arc::new(TurnMetrics::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let running = tokio::spawn(execute_advisor(
            limiter.clone(),
            metrics.clone(),
            input(Duration::from_secs(1)),
            move |_, _, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok::<_, &'static str>(("NOTHING".into(), "model".into()))
            },
        ));
        started_rx.await.expect("provider call must start");
        assert_eq!(limiter.available_permits(), 0, "provider call holds permit");

        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_for_call = invoked.clone();
        let saturated = execute_advisor(
            limiter.clone(),
            metrics.clone(),
            input(Duration::from_secs(1)),
            move |_, _, _| {
                invoked_for_call.store(true, Ordering::SeqCst);
                async { Ok::<_, &'static str>(("NOTHING".into(), "model".into())) }
            },
        )
        .await;
        assert_eq!(saturated, AdvisorExecution::Saturated);
        assert!(!invoked.load(Ordering::SeqCst));

        release_tx.send(()).expect("release running provider");
        assert_eq!(
            running.await.expect("advisor task joins"),
            AdvisorExecution::Suppressed
        );
        assert_eq!(limiter.available_permits(), 1);

        let body = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            labelled_value(&body, "ocean_advisor_outcomes_total{outcome=\"saturated\"}"),
            Some(1)
        );
        assert_eq!(
            labelled_value(
                &body,
                "ocean_advisor_outcomes_total{outcome=\"suppressed\"}"
            ),
            Some(1)
        );
    }

    #[tokio::test]
    async fn provider_error_is_accounted_without_requiring_or_formatting_error_content() {
        struct SecretProviderError {
            _secret: &'static str,
        }

        let metrics = Arc::new(TurnMetrics::default());
        let result = execute_advisor(
            limiter(2),
            metrics.clone(),
            input(Duration::from_secs(1)),
            |_, _, _| async {
                Err::<(String, String), _>(SecretProviderError {
                    _secret: "must-never-be-formatted-or-logged",
                })
            },
        )
        .await;
        assert_eq!(result, AdvisorExecution::ProviderError);
        let body = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            labelled_value(
                &body,
                "ocean_advisor_outcomes_total{outcome=\"provider_error\"}"
            ),
            Some(1)
        );
    }
}
