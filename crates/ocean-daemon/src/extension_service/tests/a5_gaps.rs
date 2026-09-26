//! Stage A5 closures for §19 rows the A1–A4 suites proved only partially:
//! every reset reason by name, the full control-lane priority order, the
//! rolling failure window, handshake failures on the production A2b runner,
//! and a missing secret at run level. Each drives existing production code;
//! none adds behavior.

use super::*;

fn session_source(dispatcher: &LifecycleDispatcher) -> LifecycleSource {
    LifecycleSource::ExplicitSessionCreated {
        succeeded: true,
        scope: dispatcher.source_scope(None, Some(Uuid::new_v4()), None, None, None),
        stamp: crate::extension_lifecycle::event_stamp(),
        title: String::new(),
        cwd: String::new(),
    }
}

/// §7.2/§9.2: each reset reason is chosen by exactly its own rule, and
/// `oldest_available`/`latest_available` describe the epoch-eligible history.
#[test]
fn a5_replay_plan_names_every_reset_reason_exactly() {
    let boot = Uuid::new_v4();
    let epoch = Uuid::new_v4();
    let dispatcher = LifecycleDispatcher::new(boot, HashSet::new());
    dispatcher.publish(LifecycleSource::DaemonStarted {
        daemon_version: "0.1.0".to_owned(),
        stamp: crate::extension_lifecycle::event_stamp(),
    });
    let floor = dispatcher.current_sequence();
    for _ in 0..4 {
        dispatcher.publish(session_source(&dispatcher));
    }
    let scope = ActivationScope {
        global: true,
        projects: HashSet::new(),
    };
    let eligible = eligible_events(
        &dispatcher.attach(),
        &scope,
        &[LifecycleEventKind::SessionStarted],
        floor,
    );
    let sequences: Vec<u64> = eligible.iter().map(|event| event.sequence.0).collect();
    assert_eq!(sequences, [2, 3, 4, 5]);
    let cursor = |epoch, after| ResumeCursor {
        daemon_boot_id: boot,
        activation_epoch: epoch,
        after_sequence: Sequence(after),
    };
    let reason = |plan: (Option<Reset>, Vec<LifecycleEvent>)| {
        let (reset, replay) = plan;
        let reset = reset.expect("a reset");
        assert!(replay.is_empty(), "a reset never replays");
        (
            reset.reason,
            reset.oldest_available.map(|s| s.0),
            reset.latest_available.map(|s| s.0),
        )
    };

    // A valid cursor replays strictly after itself.
    let (reset, replay) = replay_plan(Some(&cursor(epoch, 3)), boot, epoch, floor, &eligible);
    assert!(reset.is_none());
    assert_eq!(
        replay.iter().map(|e| e.sequence.0).collect::<Vec<_>>(),
        [4, 5]
    );
    // Another activation epoch, even with an in-range cursor.
    assert_eq!(
        reason(replay_plan(
            Some(&cursor(Uuid::new_v4(), 3)),
            boot,
            epoch,
            floor,
            &eligible
        )),
        (ResetReason::ActivationChanged, Some(2), Some(5))
    );
    // Same epoch, cursor below the epoch's replay floor.
    assert_eq!(
        reason(replay_plan(
            Some(&cursor(epoch, floor.0 - 1)),
            boot,
            epoch,
            floor,
            &eligible
        )),
        (ResetReason::ActivationChanged, Some(2), Some(5))
    );
    // The cursor's event was evicted: it is older than all retained history.
    assert_eq!(
        reason(replay_plan(
            Some(&cursor(epoch, 2)),
            boot,
            epoch,
            floor,
            &eligible[1..]
        )),
        (ResetReason::RetentionExceeded, Some(3), Some(5))
    );
    // Inside retained history but never an eligible sequence.
    let gapped = [
        eligible[0].clone(),
        eligible[2].clone(),
        eligible[3].clone(),
    ];
    assert_eq!(
        reason(replay_plan(
            Some(&cursor(epoch, 3)),
            boot,
            epoch,
            floor,
            &gapped
        )),
        (ResetReason::InvalidCursor, Some(2), Some(5))
    );
    // A cursor beyond anything sent is also invalid, not a silent live attach.
    assert_eq!(
        reason(replay_plan(
            Some(&cursor(epoch, 99)),
            boot,
            epoch,
            floor,
            &eligible
        ))
        .0,
        ResetReason::InvalidCursor
    );
}

/// §9.2: `lag` outranks `ping`, only the newest ping nonce survives, and two
/// lags coalesce into one range with summed count and OR-ed replay flag.
#[test]
fn a5_control_lane_orders_lag_before_ping_and_coalesces_exactly() {
    let controls = ControlLane::default();
    let older = Uuid::new_v4();
    let newer = Uuid::new_v4();
    controls.ping(older);
    controls.lag(Sequence(10), Sequence(12), 3, false);
    controls.ping(newer);
    controls.lag(Sequence(8), Sequence(20), 5, true);
    match controls.pop() {
        Some(HostControl::Lag(lag)) => {
            assert_eq!((lag.first_lost.0, lag.last_lost.0), (8, 20));
            assert_eq!(lag.lost_count, 8);
            assert!(lag.replay_available);
        }
        _ => panic!("lag must be written before ping"),
    }
    match controls.pop() {
        Some(HostControl::Ping(ping)) => assert_eq!(ping.nonce, newer),
        _ => panic!("the pending ping must follow"),
    }
    assert!(controls.pop().is_none(), "the older nonce was not retained");
    // Nothing coalesces into a pending shutdown.
    controls.shutdown(ShutdownReason::Disabled);
    controls.ping(Uuid::new_v4());
    controls.lag(Sequence(30), Sequence(30), 1, true);
    controls.reset(reset_frame(ResetReason::InvalidCursor, &[]));
    assert!(matches!(controls.pop(), Some(HostControl::Shutdown(_))));
    assert!(controls.pop().is_none());
}

/// Run `exit 17` under on-failure restart with four seeded failures `age`
/// old. Returns whether the circuit opened on the first new failure.
async fn circuit_after_seeded_failures(age: Duration) -> bool {
    let (temp, mut activation) = executable_fixture("#!/bin/sh\nexit 17\n");
    install_fixture_store(&temp, &mut activation);
    activation.restart_on_failure = true;
    let Some(old) = tokio::time::Instant::now().checked_sub(age) else {
        panic!("host monotonic clock is younger than the seeded failure age");
    };
    let history = Arc::new(std::sync::Mutex::new(RestartHistory {
        failures: VecDeque::from([old; 4]),
        backoff_index: 4,
        restart_count: 4,
        ..RestartHistory::default()
    }));
    let dispatcher = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
    let status = RuntimeStatusCache::default();
    let cancel = CancellationToken::new();
    let scope = activation_scope(&activation);
    let task = tokio::spawn(run_service_with_epoch(
        activation,
        dispatcher,
        cancel.clone(),
        status.clone(),
        Uuid::new_v4(),
        Sequence(0),
        scope,
        Arc::new(std::sync::Mutex::new(ShutdownReason::Disabled)),
        Arc::clone(&history),
    ));
    let opened = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match status.snapshot().first().map(|row| row.state) {
                Some(RuntimeState::CircuitOpen) => return true,
                Some(RuntimeState::Backoff) => return false,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("the first failure was never classified");
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    let history = history.lock().unwrap();
    assert_eq!(history.restart_count, 5);
    opened
}

/// §10.4: failures older than the 60 s rolling window are pruned before the
/// five-failure threshold is counted; four in-window failures plus one more
/// open the circuit.
#[tokio::test]
async fn a5_rolling_window_prunes_failures_older_than_sixty_seconds() {
    assert!(
        circuit_after_seeded_failures(Duration::from_secs(30)).await,
        "five failures inside the window must open the circuit"
    );
    assert!(
        !circuit_after_seeded_failures(Duration::from_secs(61)).await,
        "failures older than the window still counted"
    );
}

/// A child whose post-`ready` behavior is `after_ready`, run on the production
/// A2b path with no restart. Returns the terminal reason and whether the child
/// ever read a `ready` frame.
async fn production_handshake(
    service_hello: &str,
    after_ready: &str,
    events: Vec<LifecycleEventKind>,
    startup_timeout: Duration,
) -> (Option<RuntimeReason>, bool) {
    let script = format!(
        "#!/bin/sh\nIFS= read -r hello\n{service_hello}\nIFS= read -r ready || exit 0\ncase \"$ready\" in *'\"frame\":\"ready\"'*) printf ready > \"$HOME/got-ready\" ;; esac\n{after_ready}\nwhile IFS= read -r frame; do case \"$frame\" in *'\"frame\":\"shutdown\"'*) printf '%s\\n' '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"shutdown_complete\"}}'; exit 0 ;; esac; done\n"
    );
    let (temp, mut activation) = executable_fixture(&script);
    install_fixture_store(&temp, &mut activation);
    activation.events = events;
    activation.startup_timeout = startup_timeout;
    let data = activation
        .config_dir
        .join("extensions/state/example.noop/data/got-ready");
    let status = RuntimeStatusCache::default();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_service_a2b(
        activation,
        LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new()),
        cancel.clone(),
        status.clone(),
    ));
    let terminal = wait_for_runtime_state(&status, RuntimeState::Unhealthy).await;
    assert_eq!(terminal.pid, None);
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    (terminal.reason, data.exists())
}

const HELLO: &str = r#"printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started"],"resume":null}'"#;

/// §7.1–§7.2 on the production runner: a second `service_hello` after ready,
/// an expanded subscription, an unsupported version, and a hello that arrives
/// after `startup_timeout` all fail, and the three pre-ready cases never reach
/// `ready`.
#[tokio::test]
async fn a5_production_handshake_rejects_duplicate_expanded_versioned_and_late_hello() {
    let (reason, ready) = production_handshake(
        HELLO,
        HELLO,
        vec![LifecycleEventKind::DaemonStarted],
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(reason, Some(RuntimeReason::ProtocolViolation));
    assert!(ready, "the duplicate followed a real ready");

    let expanded = r#"printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started","turn_started"],"resume":null}'"#;
    let (reason, ready) = production_handshake(
        expanded,
        "",
        vec![LifecycleEventKind::DaemonStarted],
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(reason, Some(RuntimeReason::ProtocolViolation));
    assert!(!ready, "an expanded subscription reached ready");

    let versioned = HELLO.replace("\"version\":1", "\"version\":2");
    let (reason, ready) = production_handshake(
        &versioned,
        "",
        vec![LifecycleEventKind::DaemonStarted],
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(reason, Some(RuntimeReason::ProtocolViolation));
    assert!(!ready, "an unsupported version reached ready");

    let late = format!("sleep 2\n{HELLO}");
    let (reason, ready) = production_handshake(
        &late,
        "",
        vec![LifecycleEventKind::DaemonStarted],
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(reason, Some(RuntimeReason::StartupTimeout));
    assert!(!ready, "a late hello reached ready");
}

/// §11.4: a bound secret whose source is absent fails activation before any
/// child exists, with the fixed `secret_missing` reason and no temp root.
#[tokio::test]
async fn a5_missing_secret_source_fails_before_spawn() {
    let (temp, mut activation) =
        executable_fixture("#!/bin/sh\nprintf spawned > \"$HOME/spawned\"\nexit 0\n");
    install_fixture_store(&temp, &mut activation);
    let absent = format!("A5_ABSENT_{}", Uuid::new_v4().simple()).to_uppercase();
    assert!(std::env::var_os(&absent).is_none());
    activation.secret_bindings = vec![SecretBinding {
        target_env: "A5_TARGET".to_owned(),
        reference: format!("env:{absent}"),
    }];
    let config = activation.config_dir.clone();
    let status = RuntimeStatusCache::default();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_service_a2b(
        activation,
        LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new()),
        cancel.clone(),
        status.clone(),
    ));
    let terminal = wait_for_runtime_state(&status, RuntimeState::Unhealthy).await;
    assert_eq!(terminal.reason, Some(RuntimeReason::SecretMissing));
    assert_eq!(terminal.pid, None);
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    let state = config.join("extensions/state/example.noop");
    assert!(!state.join("data/spawned").exists(), "a child was spawned");
    assert!(fs::read_dir(state.join("tmp/lifecycle"))
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true));
}

/// §7.2/§19.1: a child cannot publish, command, or cancel through the
/// observer channel. Publish-shaped and command-shaped frames after ready are
/// protocol violations, and the dispatcher's sequence is untouched.
#[tokio::test]
async fn a5_observer_frames_cannot_publish_or_command() {
    for frame in [
        r#"{"protocol":"ocean.extension.service","version":1,"frame":"event","daemon_boot_id":"00000000-0000-4000-8000-000000000000","sequence":"9","event_id":"00000000-0000-4000-8000-000000000001","occurred_at":"2026-07-27T18:00:00.000Z","kind":"turn_started","scope":{"project_id":null,"session_id":null,"turn_id":null,"request_id":null,"tool_call_id":null,"permission_id":null},"metadata":{}}"#,
        r#"{"protocol":"ocean.extension.service","version":1,"frame":"cancel","request_id":"00000000-0000-4000-8000-000000000002"}"#,
        r#"{"protocol":"ocean.extension.service","version":1,"frame":"subscribe","subscriptions":["turn_started"]}"#,
    ] {
        let script = format!(
            "#!/bin/sh\nIFS= read -r hello\n{HELLO}\nIFS= read -r ready\nprintf '%s\\n' '{frame}'\nwhile IFS= read -r line; do :; done\n"
        );
        let (temp, mut activation) = executable_fixture(&script);
        install_fixture_store(&temp, &mut activation);
        let dispatcher = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        dispatcher.publish(LifecycleSource::DaemonStarted {
            daemon_version: "0.1.0".to_owned(),
            stamp: crate::extension_lifecycle::event_stamp(),
        });
        let before = dispatcher.current_sequence();
        let status = RuntimeStatusCache::default();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_service_a2b(
            activation,
            Arc::clone(&dispatcher),
            cancel.clone(),
            status.clone(),
        ));
        let terminal = wait_for_runtime_state(&status, RuntimeState::Unhealthy).await;
        assert_eq!(
            terminal.reason,
            Some(RuntimeReason::ProtocolViolation),
            "{frame}"
        );
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert_eq!(dispatcher.current_sequence(), before, "{frame}");
        assert_eq!(dispatcher.attach().retained.len(), 1);
    }
}

fn drain(rx: &mut mpsc::Receiver<QueuedEvent>) -> Vec<u64> {
    let mut sequences = Vec::new();
    while let Ok(queued) = rx.try_recv() {
        sequences.push(queued.event.sequence.0);
    }
    sequences
}

/// §9.2: the per-service data queue holds at most 256 frames and 1 MiB. What
/// does not fit coalesces into one `lag` whose `replay_available` is computed
/// from the retained, eligible history — true while the ring still holds the
/// range, false once it is evicted or outside the activation scope.
#[tokio::test]
async fn a5_live_queue_bounds_count_and_bytes_with_computed_replay_availability() {
    let dispatcher = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
    dispatcher.publish(LifecycleSource::DaemonStarted {
        daemon_version: "0.1.0".to_owned(),
        stamp: crate::extension_lifecycle::event_stamp(),
    });
    let scope = ActivationScope {
        global: true,
        projects: HashSet::new(),
    };
    let subscriptions = vec![LifecycleEventKind::SessionStarted];
    let feed = |queued_bytes: usize| {
        let attach = dispatcher.attach();
        let (tx, rx) = mpsc::channel(OUTBOUND_MAX_MESSAGES);
        let bytes = Arc::new(AtomicUsize::new(queued_bytes));
        let controls = Arc::new(ControlLane::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(feed_live_events(
            Arc::clone(&dispatcher),
            attach.live,
            attach.boundary,
            scope.clone(),
            subscriptions.clone(),
            tx,
            bytes,
            Arc::clone(&controls),
            cancel.clone(),
        ));
        (rx, controls, cancel, task)
    };
    async fn settle(controls: &ControlLane, lost: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while controls.lag_total.load(Ordering::Relaxed) < lost {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the feeder never recorded the lost range");
    }

    // Count bound: 300 eligible frames, nobody reading.
    let (mut rx, controls, cancel, task) = feed(0);
    let first = dispatcher.current_sequence().0 + 1;
    for _ in 0..300 {
        dispatcher.publish(session_source(&dispatcher));
    }
    settle(&controls, 44).await;
    cancel.cancel();
    task.await.unwrap();
    let queued = drain(&mut rx);
    assert_eq!(queued.len(), OUTBOUND_MAX_MESSAGES);
    assert_eq!(queued, (first..first + 256).collect::<Vec<_>>());
    match controls.pop() {
        Some(HostControl::Lag(lag)) => {
            assert_eq!(
                (lag.first_lost.0, lag.last_lost.0),
                (first + 256, first + 299)
            );
            assert_eq!(lag.lost_count, 44);
            assert!(lag.replay_available, "the lost range is still retained");
        }
        _ => panic!("one coalesced lag"),
    }
    assert!(controls.pop().is_none());

    // Byte bound: with the queue's bytes one short of 1 MiB, the next frame
    // cannot be reserved even though the count has room.
    let (mut rx, controls, cancel, task) = feed(OUTBOUND_MAX_BYTES - 1);
    let next = dispatcher.current_sequence().0 + 1;
    dispatcher.publish(session_source(&dispatcher));
    settle(&controls, 1).await;
    cancel.cancel();
    task.await.unwrap();
    assert!(drain(&mut rx).is_empty(), "a frame exceeded the byte bound");
    match controls.pop() {
        Some(HostControl::Lag(lag)) => {
            assert_eq!(
                (lag.first_lost.0, lag.last_lost.0, lag.lost_count),
                (next, next, 1)
            );
        }
        _ => panic!("the over-budget frame must lag"),
    }

    // replay_available is computed: evicted and out-of-scope ranges are false.
    for _ in 0..crate::extension_lifecycle::BOOT_RING_MAX_EVENTS {
        dispatcher.publish(session_source(&dispatcher));
    }
    let latest = dispatcher.current_sequence();
    assert!(dispatcher.replay_available(&scope, &subscriptions, latest, latest));
    assert!(!dispatcher.replay_available(
        &scope,
        &subscriptions,
        Sequence(first),
        Sequence(first + 299)
    ));
    let elsewhere = ActivationScope {
        global: false,
        projects: HashSet::from([Uuid::new_v4()]),
    };
    assert!(!dispatcher.replay_available(&elsewhere, &subscriptions, latest, latest));
    assert!(!dispatcher.replay_available(
        &scope,
        &[LifecycleEventKind::TurnStarted],
        latest,
        latest
    ));
}

/// §10.4 stable reset, in real time: a service that stays healthy for five
/// continuous minutes clears its failure history, so the next failure waits
/// the first backoff step (250 ms) instead of the seeded fifth (4 s). Ignored
/// because it runs about 5.5 minutes; the A5 evidence record carries its run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real time: about 5.5 minutes"]
async fn a5_stable_reset_after_five_healthy_minutes_restores_the_first_backoff() {
    let script = r#"#!/bin/sh
printf '%s\n' "$$" >> "$HOME/starts"
IFS= read -r hello
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started"],"resume":null}'
IFS= read -r ready
while IFS= read -r frame; do
  case "$frame" in
    *'"frame":"ping"'*)
      if [ -e "$HOME/exit-now" ]; then rm -f "$HOME/exit-now"; exit 17; fi
      nonce=$(printf '%s' "$frame" | sed -n 's/.*"nonce":"\([^"]*\)".*/\1/p')
      printf '{"protocol":"ocean.extension.service","version":1,"frame":"pong","nonce":"%s"}\n' "$nonce" ;;
    *'"frame":"shutdown"'*)
      printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'
      exit 0 ;;
  esac
done
"#;
    let (temp, mut activation) = executable_fixture(script);
    install_fixture_store(&temp, &mut activation);
    activation.restart_on_failure = true;
    let data = activation
        .config_dir
        .join("extensions/state/example.noop/data");
    let history = Arc::new(std::sync::Mutex::new(RestartHistory {
        failures: VecDeque::from([tokio::time::Instant::now(); 4]),
        backoff_index: 4,
        restart_count: 4,
        ..RestartHistory::default()
    }));
    let status = RuntimeStatusCache::default();
    let cancel = CancellationToken::new();
    let scope = activation_scope(&activation);
    let task = tokio::spawn(run_service_with_epoch(
        activation,
        LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new()),
        cancel.clone(),
        status.clone(),
        Uuid::new_v4(),
        Sequence(0),
        scope,
        Arc::new(std::sync::Mutex::new(ShutdownReason::Disabled)),
        Arc::clone(&history),
    ));
    wait_for_runtime_state(&status, RuntimeState::Healthy).await;
    tokio::time::sleep(STABLE_RESET + Duration::from_secs(5)).await;
    let starts = || {
        fs::read_to_string(data.join("starts"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(starts(), 1);
    fs::write(data.join("exit-now"), "").unwrap();
    // The child exits on its next ping (at most one 10 s interval away).
    let failed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if status
                .snapshot()
                .first()
                .is_some_and(|row| row.state == RuntimeState::Backoff)
            {
                return tokio::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the stable service never failed");
    tokio::time::timeout(Duration::from_secs(10), async {
        while starts() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no restart");
    let gap = failed.elapsed();
    assert!(
        gap < Duration::from_secs(2),
        "backoff {gap:?} was not reset to 250 ms"
    );
    {
        let history = history.lock().unwrap();
        assert_eq!(history.backoff_index, 1);
        assert_eq!(history.failures.len(), 1);
        assert!(!history.circuit_open);
    }
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .is_none());
}
