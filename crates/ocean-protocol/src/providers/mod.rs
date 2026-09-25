pub mod anthropic;
pub mod codex;
pub mod google;
pub mod openai;

use async_trait::async_trait;

use crate::error::Result;
use crate::stream::AssistantMessageEventStream;
use crate::types::{Context, Model, StreamOptions};

/// Generic provider interface — invoked by `stream_simple` based on `model.api`.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream>;
}

/// Minimizer M2 schema-portability fixture: the plain-object optional
/// `command`/`argv` Bash schema (no `oneOf`/`anyOf`/`required`) that
/// `ocean-runtime` offers in argv mode. Each provider's request builder must
/// carry it verbatim, with no provider-specific rewriting.
#[cfg(test)]
pub(crate) fn bash_argv_fixture() -> (crate::types::Tool, Context) {
    let parameters: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/bash_argv_tool_schema.json"
    ))
    .expect("fixture is valid JSON");
    let tool = crate::types::Tool {
        name: "bash".into(),
        description: "Run a command.".into(),
        parameters,
    };
    let context = Context {
        system_prompt: None,
        messages: vec![crate::types::Message::user_text("run it")],
        tools: vec![tool.clone()],
        dynamic_tool_declarations: Vec::new(),
        tool_choice: crate::types::ToolChoice::Auto,
    };
    (tool, context)
}
