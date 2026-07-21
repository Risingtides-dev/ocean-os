# ocean-protocol

Unified multi-provider LLM protocol for Ocean OS. Streams from Anthropic, OpenAI, Google Gemini, and any OpenAI-compatible endpoint.

## Providers

| API string | Implemented by | Endpoints |
|------------|---------------|-----------|
| `anthropic-messages` | `AnthropicProvider` | Anthropic Messages SSE |
| `openai-completions` | `OpenAiProvider` | OpenAI Chat Completions + any OpenAI-compatible base URL (OpenRouter, Together, Groq, Cerebras, DeepSeek, Fireworks, xAI, etc.) |
| `google-generative-ai` | `GoogleProvider` | Gemini `streamGenerateContent?alt=sse` |

All three stream via SSE, emit per-block `text_delta` / `thinking_delta` / `toolcall_delta` events, share a retry helper with `Retry-After` parsing, and honor a `CancellationToken` for mid-stream abort.

## Quick start

```rust
use ocean_protocol::{stream_simple, Context, Message, Model, StreamOptions};
use futures::StreamExt;

# tokio_test::block_on(async {
let model = Model::anthropic_claude_sonnet_4_6();
let ctx = Context {
    system_prompt: Some("You are a helpful assistant.".into()),
    messages: vec![Message::user_text("Say hi in one word.")],
    tools: vec![],
};
let mut events = stream_simple(&model, &ctx, &StreamOptions::default()).await.unwrap();
while let Some(ev) = events.next().await {
    println!("{:?}", ev.unwrap());
}
# });
```

`ANTHROPIC_API_KEY` (or `OPENAI_API_KEY` / `GOOGLE_API_KEY` / `GEMINI_API_KEY`) is read from the environment unless `StreamOptions::api_key` is set.

## OpenAI-compatible passthrough

```rust
use ocean_protocol::Model;
let m = Model::openai_compat(
    "openrouter",
    "anthropic/claude-3.5-sonnet",
    "https://openrouter.ai/api/v1",
    200_000, 8_192,
);
```

Set `OPENAI_API_KEY` to the provider's key, or pass `api_key` directly.

## License

MIT OR Apache-2.0. See [`/NOTICE.md`](../../NOTICE.md) for third-party attributions.
