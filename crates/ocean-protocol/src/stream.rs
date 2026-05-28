use futures::stream::BoxStream;

use crate::error::Result;
use crate::types::AssistantMessageEvent;

/// A stream of `AssistantMessageEvent`s, terminating with either `Done` or `Error`.
///
/// Equivalent to `AssistantMessageEventStream` in ocean-protocol TS.
pub type AssistantMessageEventStream = BoxStream<'static, Result<AssistantMessageEvent>>;
