use ocean_agent_sdk::extension_lifecycle::{
    decode_frame, encode_frame, DaemonVersion, FrameError, HostHello, LifecycleEvent,
    LifecycleEventKind, ServiceHello, ToolName, MAX_DAEMON_VERSION_BYTES, MAX_FRAME_BYTES,
    MAX_TOOL_NAME_BYTES,
};

const HOST_HELLO: &[u8] = include_bytes!("fixtures/extension_lifecycle/host_hello.json");
const SERVICE_HELLO: &[u8] = include_bytes!("fixtures/extension_lifecycle/service_hello.json");
const EVENTS: &str = include_str!("fixtures/extension_lifecycle/events.jsonl");
const UNKNOWN_FIELD: &[u8] = include_bytes!("fixtures/extension_lifecycle/unknown_field.json");
const VERSION_2: &[u8] = include_bytes!("fixtures/extension_lifecycle/version_2.json");

#[test]
fn golden_handshake_frames_round_trip_byte_exact() {
    let host: HostHello = decode_frame(HOST_HELLO).expect("host hello");
    assert_eq!(encode_frame(&host).expect("encode"), HOST_HELLO);

    let service: ServiceHello = decode_frame(SERVICE_HELLO).expect("service hello");
    assert_eq!(encode_frame(&service).expect("encode"), SERVICE_HELLO);
    service
        .validate_subscriptions(&[
            LifecycleEventKind::DaemonStarted,
            LifecycleEventKind::TurnStarted,
            LifecycleEventKind::ToolStarted,
        ])
        .expect("exact subset");
}

#[test]
fn all_ten_closed_metadata_variants_have_byte_exact_golden_fixtures() {
    let mut kinds = Vec::new();
    for line in EVENTS.lines() {
        let mut encoded = line.as_bytes().to_vec();
        encoded.push(b'\n');
        let event: LifecycleEvent = decode_frame(&encoded).expect("event fixture");
        assert_eq!(encode_frame(&event).expect("encode"), encoded);
        kinds.push(event.kind);
    }
    assert_eq!(
        kinds,
        [
            LifecycleEventKind::DaemonStarted,
            LifecycleEventKind::SessionStarted,
            LifecycleEventKind::TurnStarted,
            LifecycleEventKind::PermissionRequested,
            LifecycleEventKind::PermissionResolved,
            LifecycleEventKind::ToolStarted,
            LifecycleEventKind::ToolFinished,
            LifecycleEventKind::TurnFinished,
            LifecycleEventKind::SessionStopped,
            LifecycleEventKind::DaemonStopping,
        ]
    );
}

#[test]
fn unknown_fields_and_unsupported_versions_are_rejected() {
    assert_eq!(
        decode_frame::<ServiceHello>(UNKNOWN_FIELD),
        Err(FrameError::InvalidFrame)
    );
    assert_eq!(
        decode_frame::<ServiceHello>(VERSION_2),
        Err(FrameError::InvalidFrame)
    );
}

#[test]
fn valid_encode_and_decode_accept_65536_and_reject_65537() {
    let mut host: HostHello = decode_frame(HOST_HELLO).expect("host hello");
    let original_len = host.identity.package_id.len();
    let base_len = encode_frame(&host).expect("base encode").len();
    host.identity.package_id = "x".repeat(MAX_FRAME_BYTES - base_len + original_len);

    let at_limit = encode_frame(&host).expect("exact-limit encode");
    assert_eq!(at_limit.len(), MAX_FRAME_BYTES);
    let decoded: HostHello = decode_frame(&at_limit).expect("exact-limit decode");
    assert_eq!(decoded, host);

    host.identity.package_id.push('x');
    assert_eq!(
        encode_frame(&host),
        Err(FrameError::TooLarge {
            encoded_bytes: MAX_FRAME_BYTES + 1
        })
    );

    let mut over_limit = at_limit;
    over_limit.insert(over_limit.len() - 3, b'x');
    assert_eq!(over_limit.len(), MAX_FRAME_BYTES + 1);
    assert_eq!(
        decode_frame::<HostHello>(&over_limit),
        Err(FrameError::TooLarge {
            encoded_bytes: MAX_FRAME_BYTES + 1
        })
    );
}

#[test]
fn framing_rejects_non_object_trailing_duplicate_line_and_invalid_utf8() {
    for invalid in [
        b"[]\n".as_slice(),
        b"{} trailing\n".as_slice(),
        b"{}\n{}\n".as_slice(),
        b" {}\n".as_slice(),
        b"{}\r\n".as_slice(),
        &[b'{', 0xff, b'}', b'\n'],
    ] {
        assert!(
            decode_frame::<ServiceHello>(invalid).is_err(),
            "{invalid:?}"
        );
    }

    let duplicate = b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}\n";
    assert_eq!(
        decode_frame::<ServiceHello>(duplicate),
        Err(FrameError::InvalidFrame)
    );
}

#[test]
fn metadata_kind_mismatch_unknown_fields_and_non_v4_event_ids_are_rejected() {
    let mismatch = EVENTS.lines().nth(1).expect("session event").replace(
        "\"metadata\":{}",
        "\"metadata\":{\"daemon_version\":\"0.1.0\"}",
    );
    let mut mismatch = mismatch.into_bytes();
    mismatch.push(b'\n');
    assert_eq!(
        decode_frame::<LifecycleEvent>(&mismatch),
        Err(FrameError::InvalidFrame)
    );

    let unknown = EVENTS.lines().next().expect("daemon event").replace(
        "\"daemon_version\":\"0.1.0\"",
        "\"daemon_version\":\"0.1.0\",\"prompt\":\"secret\"",
    );
    let mut unknown = unknown.into_bytes();
    unknown.push(b'\n');
    assert_eq!(
        decode_frame::<LifecycleEvent>(&unknown),
        Err(FrameError::InvalidFrame)
    );

    let non_v4 = EVENTS.lines().next().expect("daemon event").replace(
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-0000-0000-000000000001",
    );
    let mut non_v4 = non_v4.into_bytes();
    non_v4.push(b'\n');
    assert_eq!(
        decode_frame::<LifecycleEvent>(&non_v4),
        Err(FrameError::InvalidFrame)
    );

    let mut invalid_encode: LifecycleEvent = {
        let mut first = EVENTS
            .lines()
            .next()
            .expect("daemon event")
            .as_bytes()
            .to_vec();
        first.push(b'\n');
        decode_frame(&first).expect("valid event")
    };
    invalid_encode.kind = LifecycleEventKind::TurnStarted;
    assert_eq!(encode_frame(&invalid_encode), Err(FrameError::InvalidFrame));
}

#[test]
fn duplicate_keys_are_rejected_in_every_nested_object_boundary() {
    let host = std::str::from_utf8(HOST_HELLO).expect("host utf8");
    let duplicate_identity = host.replace(
        "\"package_id\":\"example.noop\"",
        "\"package_id\":\"example.noop\",\"package_id\":\"forged\"",
    );
    let duplicate_limits = host.replace(
        "\"max_frame_bytes\":65536",
        "\"max_frame_bytes\":65536,\"max_frame_bytes\":1",
    );
    for invalid in [duplicate_identity, duplicate_limits] {
        assert_eq!(
            decode_frame::<HostHello>(invalid.as_bytes()),
            Err(FrameError::InvalidFrame)
        );
    }

    let duplicate_resume = b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":{\"daemon_boot_id\":\"22222222-2222-4222-8222-222222222222\",\"activation_epoch\":\"33333333-3333-4333-8333-333333333333\",\"after_sequence\":\"41\",\"after_sequence\":\"42\"}}\n";
    assert_eq!(
        decode_frame::<ServiceHello>(duplicate_resume),
        Err(FrameError::InvalidFrame)
    );

    for line in EVENTS.lines() {
        let duplicate_scope = line.replacen(
            "\"scope\":{",
            "\"scope\":{\"request_id\":null,\"request_id\":null,",
            1,
        );
        let mut duplicate_scope = duplicate_scope.into_bytes();
        duplicate_scope.push(b'\n');
        assert_eq!(
            decode_frame::<LifecycleEvent>(&duplicate_scope),
            Err(FrameError::InvalidFrame)
        );

        let duplicate_metadata = line.replacen(
            "\"metadata\":{",
            "\"metadata\":{\"duplicate\":null,\"duplicate\":null,",
            1,
        );
        let mut duplicate_metadata = duplicate_metadata.into_bytes();
        duplicate_metadata.push(b'\n');
        assert_eq!(
            decode_frame::<LifecycleEvent>(&duplicate_metadata),
            Err(FrameError::InvalidFrame)
        );
    }
}

#[test]
fn daemon_versions_are_semver_only_bounded_on_decode_and_construction() {
    assert_eq!(
        DaemonVersion::new("1.2.3").expect("semver").as_str(),
        "1.2.3"
    );
    assert_eq!(
        DaemonVersion::new("release-one"),
        Err(FrameError::InvalidFrame)
    );

    let invalid = EVENTS.lines().next().expect("daemon event").replace(
        "\"daemon_version\":\"0.1.0\"",
        "\"daemon_version\":\"latest\"",
    );
    let mut invalid = invalid.into_bytes();
    invalid.push(b'\n');
    assert_eq!(
        decode_frame::<LifecycleEvent>(&invalid),
        Err(FrameError::InvalidFrame)
    );

    let oversized_semver = format!("1.0.0+{}", "a".repeat(MAX_DAEMON_VERSION_BYTES));
    assert!(oversized_semver.len() > MAX_DAEMON_VERSION_BYTES);
    assert_eq!(
        DaemonVersion::new(oversized_semver.clone()),
        Err(FrameError::InvalidFrame)
    );
    let oversized = EVENTS.lines().next().expect("daemon event").replace(
        "\"daemon_version\":\"0.1.0\"",
        &format!("\"daemon_version\":\"{oversized_semver}\""),
    );
    let mut oversized = oversized.into_bytes();
    oversized.push(b'\n');
    assert_eq!(
        decode_frame::<LifecycleEvent>(&oversized),
        Err(FrameError::InvalidFrame)
    );
}

#[test]
fn parser_errors_are_fixed_and_do_not_reflect_untrusted_keys() {
    let error = decode_frame::<ServiceHello>(
        b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null,\"SECRET_ATTACKER_KEY\":true}\n",
    )
    .expect_err("unknown key");
    assert_eq!(error.to_string(), "invalid frame");
    assert!(!error.to_string().contains("SECRET_ATTACKER_KEY"));
}

#[test]
fn subscription_must_be_duplicate_free_subset_and_cannot_override_identity() {
    let duplicate: ServiceHello = decode_frame(
        b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[\"turn_started\",\"turn_started\"],\"resume\":null}\n",
    )
    .expect("shape");
    assert!(duplicate
        .validate_subscriptions(&[LifecycleEventKind::TurnStarted])
        .is_err());

    let expanded: ServiceHello = decode_frame(SERVICE_HELLO).expect("fixture");
    assert!(expanded
        .validate_subscriptions(&[LifecycleEventKind::DaemonStarted])
        .is_err());

    let identity_override = b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null,\"identity\":{}}\n";
    assert!(decode_frame::<ServiceHello>(identity_override).is_err());
}

#[test]
fn tool_names_are_bounded_by_utf8_bytes() {
    assert!(ToolName::new("x".repeat(MAX_TOOL_NAME_BYTES)).is_ok());
    assert!(ToolName::new("x".repeat(MAX_TOOL_NAME_BYTES + 1)).is_err());
    assert!(ToolName::new("é".repeat(MAX_TOOL_NAME_BYTES / 2)).is_ok());
    assert!(ToolName::new(format!("{}x", "é".repeat(MAX_TOOL_NAME_BYTES / 2))).is_err());
}

#[test]
fn lifecycle_wire_contains_no_forbidden_payload_field_names() {
    let forbidden = [
        "prompt",
        "transcript",
        "thinking",
        "arguments",
        "args",
        "result",
        "secret",
        "environment",
        "headers",
        "cwd",
        "path",
        "title",
        "error",
        "canvas",
        "payload",
    ];
    for line in EVENTS.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("fixture json");
        let object = value.as_object().expect("object");
        let keys = object
            .keys()
            .chain(object["scope"].as_object().expect("scope").keys())
            .chain(object["metadata"].as_object().expect("metadata").keys());
        for key in keys {
            assert!(!forbidden.contains(&key.as_str()), "forbidden key {key}");
        }
    }
}
