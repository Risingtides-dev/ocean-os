use ocean_agent_sdk::extension_lifecycle::{
    decode_frame, encode_frame, FrameError, HostHello, LifecycleEvent, LifecycleEventKind,
    ServiceHello, ToolName, MAX_FRAME_BYTES, MAX_TOOL_NAME_BYTES,
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
    assert!(matches!(
        decode_frame::<ServiceHello>(UNKNOWN_FIELD),
        Err(FrameError::InvalidFrame(_))
    ));
    assert!(matches!(
        decode_frame::<ServiceHello>(VERSION_2),
        Err(FrameError::InvalidFrame(_))
    ));
}

#[test]
fn encoded_byte_limit_accepts_65536_and_rejects_65537_before_json_decode() {
    let prefix = br#"{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null,"padding":""#;
    let suffix = b"\"}\n";
    let mut at_limit = Vec::with_capacity(MAX_FRAME_BYTES);
    at_limit.extend_from_slice(prefix);
    at_limit.resize(MAX_FRAME_BYTES - suffix.len(), b'x');
    at_limit.extend_from_slice(suffix);
    assert_eq!(at_limit.len(), MAX_FRAME_BYTES);
    assert!(matches!(
        decode_frame::<ServiceHello>(&at_limit),
        Err(FrameError::InvalidFrame(_))
    ));

    let mut over_limit = at_limit;
    over_limit.insert(over_limit.len() - suffix.len(), b'x');
    assert_eq!(over_limit.len(), MAX_FRAME_BYTES + 1);
    assert_eq!(
        decode_frame::<ServiceHello>(&over_limit),
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
    assert!(matches!(
        decode_frame::<ServiceHello>(duplicate),
        Err(FrameError::InvalidFrame(_))
    ));
}

#[test]
fn metadata_kind_mismatch_unknown_fields_and_non_v4_event_ids_are_rejected() {
    let mismatch = EVENTS.lines().nth(1).expect("session event").replace(
        "\"metadata\":{}",
        "\"metadata\":{\"daemon_version\":\"0.1.0\"}",
    );
    let mut mismatch = mismatch.into_bytes();
    mismatch.push(b'\n');
    assert!(matches!(
        decode_frame::<LifecycleEvent>(&mismatch),
        Err(FrameError::InvalidFrame(_))
    ));

    let unknown = EVENTS.lines().next().expect("daemon event").replace(
        "\"daemon_version\":\"0.1.0\"",
        "\"daemon_version\":\"0.1.0\",\"prompt\":\"secret\"",
    );
    let mut unknown = unknown.into_bytes();
    unknown.push(b'\n');
    assert!(matches!(
        decode_frame::<LifecycleEvent>(&unknown),
        Err(FrameError::InvalidFrame(_))
    ));

    let non_v4 = EVENTS.lines().next().expect("daemon event").replace(
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-0000-0000-000000000001",
    );
    let mut non_v4 = non_v4.into_bytes();
    non_v4.push(b'\n');
    assert!(matches!(
        decode_frame::<LifecycleEvent>(&non_v4),
        Err(FrameError::InvalidFrame(_))
    ));

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
    assert!(matches!(
        encode_frame(&invalid_encode),
        Err(FrameError::InvalidFrame(_))
    ));
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
