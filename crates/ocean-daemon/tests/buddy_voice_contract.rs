//! Consumer half of `docs/contracts/voice-wire.json` for Ocean Buddy.
//!
//! Buddy is Swift, and the Rust workspace cannot compile it, so this test
//! reads the Swift source the way `voice_wire_contract_matches_the_daemon`
//! reads the daemon's handlers: it pulls out the literals Buddy puts on the
//! wire (the client-secret route and body keys, the `purpose` it asks for,
//! the response keys it decodes, the tool names it dispatches on, and the
//! handoff route, body, role and kind) and holds each one inside the
//! published contract. A literal the scan cannot find fails the test rather
//! than passing vacuously.

use std::collections::BTreeSet;

use serde_json::Value;

const CONTRACT: &str = include_str!("../../../docs/contracts/voice-wire.json");
const SECRET_CLIENT: &str = include_str!(
    "../../../integrations/ocean-buddy/Sources/OceanBuddyCore/RealtimeSecretClient.swift"
);
const MODELS: &str =
    include_str!("../../../integrations/ocean-buddy/Sources/OceanBuddyCore/RealtimeModels.swift");
const BROKER: &str = include_str!(
    "../../../integrations/ocean-buddy/Sources/OceanBuddyCore/RealtimeToolBroker.swift"
);

fn contract() -> Value {
    serde_json::from_str(CONTRACT).expect("voice-wire.json parses")
}

fn list(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{value} is a list"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// The text from `start` up to (not including) the next `end`.
fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("Buddy source has {start:?}"));
    let rest = &source[from..];
    let to = rest
        .find(end)
        .unwrap_or_else(|| panic!("{end:?} follows {start:?}"));
    &rest[..to]
}

/// The string literal right after each `needle`.
fn quoted_after(haystack: &str, needle: &str) -> Vec<String> {
    haystack
        .match_indices(needle)
        .map(|(at, lit)| {
            let rest = &haystack[at + lit.len()..];
            rest[..rest.find('"').unwrap()].to_string()
        })
        .collect()
}

/// The string literals in a line, ignoring anything after the first `:`.
fn case_literals(line: &str) -> Vec<String> {
    line.split(':')
        .next()
        .unwrap()
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The names a Swift `switch` dispatches on: every `case "…"` label.
fn switch_cases(body: &str) -> BTreeSet<String> {
    let cases: BTreeSet<String> = body
        .lines()
        .filter(|line| line.trim_start().starts_with("case \""))
        .flat_map(case_literals)
        .collect();
    assert!(!cases.is_empty(), "the scan found the switch cases");
    cases
}

/// `METHOD /path` for a URLRequest built from an `appendingPathComponent`
/// chain. A non-literal component is a path parameter, written `{id}`.
fn route(body: &str) -> String {
    let path: Vec<String> = body
        .match_indices(".appendingPathComponent(")
        .map(|(at, lit)| {
            let rest = &body[at + lit.len()..];
            let arg = &rest[..rest.find(')').unwrap()];
            match arg.strip_prefix('"') {
                Some(literal) => literal.trim_end_matches('"').to_string(),
                None => "{id}".to_string(),
            }
        })
        .collect();
    assert!(!path.is_empty(), "the scan found the path components");
    let method = quoted_after(body, "request.httpMethod = \"");
    assert_eq!(method.len(), 1, "one HTTP method");
    format!("{} /{}", method[0], path.join("/"))
}

/// The `"key": value` pairs of a Swift dictionary literal. A value that is
/// not a string literal comes back as `None`.
fn dictionary(literal: &str) -> Vec<(String, Option<String>)> {
    let pairs: Vec<(String, Option<String>)> = literal
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once(':')?;
            let key = key.trim().trim_start_matches('[').trim();
            let key = key.strip_prefix('"')?.strip_suffix('"')?.to_string();
            let value = value.trim().trim_end_matches(']').trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map(str::to_string);
            Some((key, value))
        })
        .collect();
    assert!(!pairs.is_empty(), "the scan found the dictionary pairs");
    pairs
}

/// `POST /v1/voice/realtime/client-secret`: Buddy mints on a published route,
/// sends only published request fields, asks for a published purpose, and
/// reads the published error key off a failure.
#[test]
fn buddy_client_secret_request_is_inside_the_published_voice_wire() {
    let wire = contract();
    let realtime = &wire["realtime"];

    let endpoint = section(SECRET_CLIENT, "endpoint = baseURL", "endpointAllowed =");
    let mint = section(
        SECRET_CLIENT,
        "public func mint(",
        "let secret = try JSONDecoder()",
    );
    let method = section(mint, "request.httpMethod", "\n");
    let route = route(&format!("{endpoint}{method}"));
    assert!(
        list(&wire["routes"]).contains(&route),
        "Buddy mints on unpublished route {route}"
    );

    let initial = section(mint, "var body: [String: String] = [", "\n");
    let mut sent = dictionary(&initial["var body: [String: String] = ".len()..]);
    sent.extend(
        quoted_after(mint, "body[\"")
            .into_iter()
            .map(|key| (key, None)),
    );
    let fields = list(&realtime["request_fields"]);
    for (key, _) in &sent {
        assert!(
            fields.contains(key),
            "Buddy sends unpublished client-secret field {key}"
        );
    }
    let purposes: Vec<&String> = sent
        .iter()
        .filter(|(key, _)| key == "purpose")
        .filter_map(|(_, value)| value.as_ref())
        .collect();
    assert_eq!(purposes.len(), 1, "Buddy asks for one literal purpose");
    assert!(
        list(&realtime["purposes"]).contains(purposes[0]),
        "Buddy asks for unpublished purpose {}",
        purposes[0]
    );

    assert_eq!(
        quoted_after(mint, "upstream?[\""),
        vec![wire["error_key"].as_str().unwrap().to_string()],
        "Buddy reads the published error key"
    );
}

/// The mint response Buddy decodes: every key `BuddyRealtimeSecret` requires
/// is a published response key, and an optional one is published either as a
/// response key or as the conversation workspace key the daemon adds.
#[test]
fn buddy_client_secret_response_is_inside_the_published_voice_wire() {
    let wire = contract();
    let realtime = &wire["realtime"];
    let response_keys = list(&realtime["response_keys"]);
    let workspace_key = realtime["conversation_workspace_key"].as_str().unwrap();

    let secret = section(MODELS, "public struct BuddyRealtimeSecret", "func expires(");
    let coding_keys = section(secret, "enum CodingKeys", "}");
    let mut checked = 0;
    for line in coding_keys.lines() {
        let Some(case) = line.trim().strip_prefix("case ") else {
            continue;
        };
        let (property, wire_key) = match case.split_once('=') {
            Some((property, raw)) => (property.trim(), raw.trim().trim_matches('"').to_string()),
            None => (case.trim(), case.trim().to_string()),
        };
        let declaration = secret
            .lines()
            .find(|line| line.trim().starts_with(&format!("public let {property}: ")))
            .unwrap_or_else(|| panic!("{property} is declared"));
        let optional = declaration.trim_end().ends_with('?');
        if optional {
            assert!(
                response_keys.contains(&wire_key) || wire_key == workspace_key,
                "Buddy decodes unpublished optional key {wire_key}"
            );
        } else {
            assert!(
                response_keys.contains(&wire_key),
                "Buddy requires unpublished key {wire_key}"
            );
        }
        checked += 1;
    }
    assert!(checked >= 2, "the scan found the CodingKeys cases");
}

/// The tools Buddy dispatches on are exactly the tools the daemon can hand a
/// session minted with Buddy's purpose (with or without a workspace), so a
/// tool the daemon adds or renames fails here instead of reaching Buddy's
/// "unavailable" default. The quota only counts tools of that set.
#[test]
fn buddy_tool_names_match_the_published_voice_wire() {
    let wire = contract();
    let tools = wire["realtime"]["tools"].as_object().unwrap();
    let purpose = quoted_after(SECRET_CLIENT, "[\"purpose\": \"");
    assert_eq!(purpose.len(), 1, "Buddy asks for one literal purpose");
    let purpose = &purpose[0];
    let reachable: BTreeSet<String> = tools
        .iter()
        .filter(|(mode, _)| *mode == purpose || mode.starts_with(&format!("{purpose}_")))
        .flat_map(|(_, names)| list(names))
        .collect();
    assert!(!reachable.is_empty(), "{purpose} mints publish tools");

    let fulfill = section(BROKER, "public func fulfill(", "private func writeHandoff(");
    assert!(
        fulfill.contains("default:"),
        "an unknown tool still gets an answer"
    );
    assert_eq!(switch_cases(fulfill), reachable);

    let quota = section(BROKER, "mutating func consume(", "mutating func reset(");
    for name in switch_cases(quota) {
        assert!(
            reachable.contains(&name),
            "Buddy's quota counts unpublished tool {name}"
        );
    }
}

/// `write_handoff`: Buddy appends on the published handoff route with only
/// published fields, a published role and kind, and treats only the
/// published `ok` key as an acknowledgement.
#[test]
fn buddy_handoff_is_inside_the_published_voice_wire() {
    let wire = contract();
    let handoff = &wire["handoff"];

    let write = section(
        BROKER,
        "private func writeHandoff(",
        "private func jsonOutput(",
    );
    let endpoint = section(write, "let endpoint = baseURL", "request.timeoutInterval");
    assert_eq!(route(endpoint), wire["handoff_route"].as_str().unwrap());

    let body = section(write, "withJSONObject: [", "])");
    let sent = dictionary(&body["withJSONObject: ".len()..]);
    let fields = list(&handoff["request_fields"]);
    for (key, _) in &sent {
        assert!(
            fields.contains(key),
            "Buddy sends unpublished handoff field {key}"
        );
    }
    let literal = |key: &str| -> String {
        sent.iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("Buddy sends a literal {key}"))
    };
    let role = literal("role");
    assert!(
        list(&handoff["roles"]).contains(&role),
        "Buddy sends unpublished handoff role {role}"
    );
    let kind = literal("kind");
    assert!(
        list(&handoff["kinds"]).contains(&kind),
        "Buddy sends unpublished handoff kind {kind}"
    );

    let acknowledgement = section(BROKER, "struct BuddyHandoffAcknowledgement", "/// Converts");
    let read = quoted_after(acknowledgement, "object[\"");
    assert!(!read.is_empty(), "the scan found the acknowledgement key");
    let response_keys = list(&handoff["response_keys"]);
    for key in read {
        assert!(
            response_keys.contains(&key),
            "Buddy reads unpublished handoff key {key}"
        );
    }
}
