# Ocean native internals dependency map

## Scope

Current dependency surface in `crates/ocean-agent`.
Only `pi-agent` and `pi-ai` are still borrowed from Pi Rust crates.

## Borrowed surfaces

| Surface | Current use | Next Ocean crate |
|---|---|---|
| `pi_agent::run_agent_with_history` | owns the actual prompt loop and message replay | deeper `ocean-agent` loop ownership |
| `pi_agent::tools::default_tools` | supplies the default file/shell tool set | `ocean-tools` |
| `pi_agent::{AgentConfig, AgentEvent}` | configures turns/streaming and maps tool/text events | deeper `ocean-agent` loop ownership |
| `pi_agent::{PermissionDecision, PermissionPolicy}` | daemon-side allow/deny policy hook | `ocean-agent` + protocol-owned permission shapes |
| `pi_ai::Model` | provider/model description and factory helpers | `ocean-providers` |
| `pi_ai::{Message, Content}` | session history format and assistant text extraction | `ocean-store` and later Ocean-owned message model |
| `pi_ai::now_ms` | session timestamps | `ocean-store` |

## What Ocean already owns

- request/session normalization in `crates/ocean-agent`
- daemon-safe backend naming (`ocean-native-deepseek`)
- config-dir resolution and DeepSeek auth fallback
- session file load/save/list logic
- prompt-to-response shaping for the HTTP API
- fallback extraction of the last assistant text for smoke behavior

## Extraction order

1. **`ocean-store` first**
   - move session persistence out before touching the live agent loop
   - keep the JSON session shape stable so existing sessions still load
   - no change to prompt output expected

2. **`ocean-providers` second**
   - move model selection, provider IDs, and API-key lookup behind an Ocean-owned provider boundary
   - keep the same default provider/model values so `ocean-rs prompt` still hits the same backend

3. **`ocean-tools` third**
   - move the default tool catalog and mutating-tool policy into Ocean-owned code
   - keep tool names, arguments, and approval semantics unchanged until the daemon protocol is ready

4. **Deeper `ocean-agent` loop ownership last**
   - replace `pi-agent::run_agent_with_history` with an Ocean loop after providers/tools/store exist
   - only then change event synthesis, streaming, and permission-request handling

This order keeps the current `ocean-rs prompt "Reply exactly: OCEAN_OK"` smoke path intact until each replacement has a parity test.

## Tests needed before each extraction

### Before `ocean-store`
- session JSON round-trip test
- load-old-session compatibility test
- `ocean-rs prompt` smoke with an existing session ID

### Before `ocean-providers`
- model selection matrix test (`OCEAN_MODEL`, `PI_MODEL`, default)
- DeepSeek key fallback test from `~/.pi/agent/auth.json`
- health/backend string regression check

### Before `ocean-tools`
- tool-list snapshot or shape test
- mutating-tool denial test with `yolo = false`
- parity check for existing tool names and args with the smoke prompt

### Before loop ownership
- fake-backend prompt test for streamed text and assistant-message finalization
- event-order test for text, tool, and permission emissions
- end-to-end `ocean-rs prompt "Reply exactly: OCEAN_OK"` smoke after the loop swap

## First obvious seam

If we take a code seam before the larger crate moves, make it `ocean-store`.
It is deterministic, already isolated in `ocean-agent`, and least likely to affect the smoke path.
