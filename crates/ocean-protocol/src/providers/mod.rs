pub mod anthropic;
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
