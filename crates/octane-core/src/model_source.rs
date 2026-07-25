//! Driving a [`LanguageModel`] as a [`StepSource`].
//!
//! This is the join between `octane-provider` and the loop. It consumes a
//! normalized [`StreamEvent`] stream, publishes protocol events as they arrive
//! so the UI can render tokens live, and accumulates the [`StepOutput`] the loop
//! needs to decide what happens next.
//!
//! Both halves matter. Publishing without accumulating would leave the loop with
//! nothing to act on; accumulating without publishing would make the response
//! appear all at once when the step ends, which is the thing streaming exists to
//! avoid.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use octane_protocol::{ItemKind, ItemStatus, Message, ToolCall, ToolCallId};
use octane_provider::model::{LanguageModel, ModelRequest, ToolSchema};
use octane_provider::stream::{FinishReason, StreamEvent};

use crate::events::EventSink;
use crate::turn::{StepOutput, StepSource};
use crate::CoreError;

pub struct ModelStepSource {
    model: Arc<dyn LanguageModel>,
    tools: Vec<ToolSchema>,
}

impl std::fmt::Debug for ModelStepSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelStepSource")
            .field("model", &self.model.info().id)
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl ModelStepSource {
    pub fn new(model: Arc<dyn LanguageModel>, tools: Vec<ToolSchema>) -> Self {
        Self { model, tools }
    }

    pub fn model(&self) -> &Arc<dyn LanguageModel> {
        &self.model
    }
}

#[async_trait]
impl StepSource for ModelStepSource {
    async fn next_step(
        &self,
        messages: &[Message],
        sink: &EventSink,
    ) -> Result<StepOutput, CoreError> {
        let info = self.model.info();
        let request = ModelRequest {
            messages: messages.to_vec(),
            tools: self.tools.clone(),
            max_output_tokens: info.max_output_tokens,
            temperature: None,
            top_p: None,
        };

        let mut stream = self.model.stream(request).await?;
        let mut step = StepOutput::default();

        // Ids of the in-flight items, so deltas address the right one.
        let mut text_item = None;
        let mut reasoning_item = None;
        let mut pending: Vec<(ToolCallId, String, String)> = Vec::new();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::StepStart => {}

                StreamEvent::TextStart => {
                    text_item =
                        Some(sink.item_started(ItemKind::AgentMessage { text: String::new() }));
                }
                StreamEvent::TextDelta(delta) => {
                    step.text.push_str(&delta);
                    // An endpoint that never sends `TextStart` — several skip it
                    // — must still stream, so the item is opened lazily.
                    let id = text_item.get_or_insert_with(|| {
                        sink.item_started(ItemKind::AgentMessage { text: String::new() })
                    });
                    sink.item_delta(id, delta);
                }
                StreamEvent::TextEnd => {
                    if let Some(id) = text_item.take() {
                        sink.item_completed(
                            id,
                            ItemKind::AgentMessage { text: step.text.clone() },
                            ItemStatus::Completed,
                        );
                    }
                }

                StreamEvent::ReasoningStart => {
                    reasoning_item =
                        Some(sink.item_started(ItemKind::Reasoning { text: String::new() }));
                }
                StreamEvent::ReasoningDelta(delta) => {
                    step.reasoning.push_str(&delta);
                    let id = reasoning_item.get_or_insert_with(|| {
                        sink.item_started(ItemKind::Reasoning { text: String::new() })
                    });
                    sink.item_delta(id, delta);
                }
                StreamEvent::ReasoningEnd => {
                    if let Some(id) = reasoning_item.take() {
                        sink.item_completed(
                            id,
                            ItemKind::Reasoning { text: step.reasoning.clone() },
                            ItemStatus::Completed,
                        );
                    }
                }

                StreamEvent::ToolCallStart { id, name } => {
                    pending.push((id, name, String::new()));
                }
                StreamEvent::ToolCallInputDelta { id, delta } => {
                    if let Some(entry) = pending.iter_mut().find(|(call, _, _)| *call == id) {
                        entry.2.push_str(&delta);
                    }
                }
                StreamEvent::ToolCallEnd { id, input } => {
                    // The end event carries the complete arguments, so it is
                    // authoritative over anything accumulated from deltas.
                    let name = pending
                        .iter()
                        .find(|(call, _, _)| *call == id)
                        .map(|(_, name, _)| name.clone())
                        .unwrap_or_default();
                    pending.retain(|(call, _, _)| *call != id);

                    sink.item(ItemKind::ToolExecution {
                        call_id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                    step.tool_calls.push(ToolCall { id, name, input });
                }

                StreamEvent::StepFinish { reason, usage } => {
                    step.wants_tools = reason.continues() || !step.tool_calls.is_empty();
                    step.truncated = reason == FinishReason::Length;
                    step.tokens_used = (usage.input_tokens + usage.output_tokens) as usize;

                    // Priced here rather than in the provider: only the caller
                    // knows which model's rates apply.
                    let mut usage = usage;
                    usage.cost = info.pricing.cost(&usage);
                    sink.usage(usage);
                }
            }
        }

        // A stream that ends without closing its blocks still has to produce a
        // coherent transcript, and several providers do exactly that.
        if let Some(id) = text_item.take() {
            sink.item_completed(
                id,
                ItemKind::AgentMessage { text: step.text.clone() },
                ItemStatus::Completed,
            );
        }
        if let Some(id) = reasoning_item.take() {
            sink.item_completed(
                id,
                ItemKind::Reasoning { text: step.reasoning.clone() },
                ItemStatus::Completed,
            );
        }

        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::{Event, ItemEvent, TurnId, Usage};
    use octane_provider::model::{ModelInfo, ProviderId};
    use octane_provider::stream::ModelStream;
    use octane_provider::ProviderError;

    /// A model that replays a fixed script.
    struct Scripted(Vec<StreamEvent>);

    #[async_trait]
    impl LanguageModel for Scripted {
        fn info(&self) -> &ModelInfo {
            static INFO: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
            INFO.get_or_init(|| ModelInfo {
                provider: ProviderId("test".into()),
                id: "m".into(),
                display_name: "M".into(),
                context_window: 200_000,
                max_output_tokens: 4_096,
                supports_tools: true,
                supports_images: false,
                supports_reasoning: true,
                needs_explicit_cache_control: false,
                pricing: octane_provider::pricing::Pricing {
                    input: 3.0,
                    output: 15.0,
                    cached_input: 0.3,
                    cache_write: 3.75,
                },
            })
        }

        async fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProviderError> {
            let events: Vec<Result<StreamEvent, ProviderError>> =
                self.0.clone().into_iter().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn estimate_tokens(&self, _messages: &[Message]) -> u64 {
            0
        }
    }

    async fn run(script: Vec<StreamEvent>) -> (StepOutput, Vec<Event>) {
        let (sink, mut events) = EventSink::new(TurnId::new());
        let source = ModelStepSource::new(Arc::new(Scripted(script)), Vec::new());
        let step = source.next_step(&[], &sink).await.unwrap();

        let mut collected = Vec::new();
        while let Ok(event) = events.try_recv() {
            collected.push(event);
        }
        (step, collected)
    }

    fn deltas(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Item(ItemEvent::Delta { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn text_is_both_published_as_deltas_and_accumulated() {
        // Publishing without accumulating leaves the loop nothing to act on;
        // accumulating without publishing defeats streaming.
        let (step, events) = run(vec![
            StreamEvent::TextStart,
            StreamEvent::TextDelta("hel".into()),
            StreamEvent::TextDelta("lo".into()),
            StreamEvent::TextEnd,
            StreamEvent::StepFinish { reason: FinishReason::Stop, usage: Usage::default() },
        ])
        .await;

        assert_eq!(step.text, "hello");
        assert_eq!(deltas(&events), vec!["hel", "lo"]);
    }

    #[tokio::test]
    async fn a_stream_that_skips_text_start_still_streams() {
        // Several endpoints never send it.
        let (step, events) = run(vec![
            StreamEvent::TextDelta("implicit".into()),
            StreamEvent::StepFinish { reason: FinishReason::Stop, usage: Usage::default() },
        ])
        .await;

        assert_eq!(step.text, "implicit");
        assert_eq!(deltas(&events), vec!["implicit"]);
        // ...and the item is still closed, or the UI leaves it pending forever.
        assert!(events.iter().any(|e| matches!(e, Event::Item(ItemEvent::Completed { .. }))));
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_closing_its_blocks_still_completes_them() {
        let (_step, events) = run(vec![
            StreamEvent::TextStart,
            StreamEvent::TextDelta("unterminated".into()),
        ])
        .await;

        assert!(events.iter().any(|e| matches!(e, Event::Item(ItemEvent::Completed { .. }))));
    }

    #[tokio::test]
    async fn tool_calls_are_collected_for_the_loop_and_announced_to_the_ui() {
        let id = ToolCallId::new();
        let (step, events) = run(vec![
            StreamEvent::ToolCallStart { id: id.clone(), name: "read".into() },
            StreamEvent::ToolCallInputDelta { id: id.clone(), delta: "{}".into() },
            StreamEvent::ToolCallEnd { id: id.clone(), input: r#"{"path":"a.rs"}"#.into() },
            StreamEvent::StepFinish { reason: FinishReason::ToolCalls, usage: Usage::default() },
        ])
        .await;

        assert_eq!(step.tool_calls.len(), 1);
        assert_eq!(step.tool_calls[0].name, "read");
        // The end event is authoritative over the accumulated deltas.
        assert_eq!(step.tool_calls[0].input, r#"{"path":"a.rs"}"#);
        assert!(step.wants_tools);

        assert!(events.iter().any(|event| matches!(
            event,
            Event::Item(ItemEvent::Completed { item, .. })
                if matches!(item.kind, ItemKind::ToolExecution { .. })
        )));
    }

    #[tokio::test]
    async fn reasoning_streams_separately_from_prose() {
        let (step, events) = run(vec![
            StreamEvent::ReasoningStart,
            StreamEvent::ReasoningDelta("thinking".into()),
            StreamEvent::ReasoningEnd,
            StreamEvent::TextStart,
            StreamEvent::TextDelta("answer".into()),
            StreamEvent::TextEnd,
            StreamEvent::StepFinish { reason: FinishReason::Stop, usage: Usage::default() },
        ])
        .await;

        assert_eq!(step.reasoning, "thinking");
        assert_eq!(step.text, "answer");

        let kinds: Vec<&'static str> = events
            .iter()
            .filter_map(|event| match event {
                Event::Item(ItemEvent::Completed { item, .. }) => Some(match item.kind {
                    ItemKind::Reasoning { .. } => "reasoning",
                    ItemKind::AgentMessage { .. } => "message",
                    _ => "other",
                }),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["reasoning", "message"]);
    }

    #[tokio::test]
    async fn usage_is_priced_before_it_is_published() {
        // Only the caller knows which model's rates apply, so the provider
        // cannot do this and leaves cost at zero.
        let (_step, events) = run(vec![StreamEvent::StepFinish {
            reason: FinishReason::Stop,
            usage: Usage { input_tokens: 1_000_000, output_tokens: 0, ..Default::default() },
        }])
        .await;

        let usage = events
            .iter()
            .find_map(|event| match event {
                Event::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("usage should be published");
        assert!((usage.cost - 3.0).abs() < 1e-6, "got {}", usage.cost);
    }

    #[tokio::test]
    async fn hitting_the_output_limit_is_reported_as_truncation() {
        let (step, _events) = run(vec![StreamEvent::StepFinish {
            reason: FinishReason::Length,
            usage: Usage::default(),
        }])
        .await;
        assert!(step.truncated);
        assert!(!step.wants_tools);
    }

    #[tokio::test]
    async fn a_plain_answer_does_not_ask_for_tools() {
        let (step, _events) = run(vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::StepFinish { reason: FinishReason::Stop, usage: Usage::default() },
        ])
        .await;
        assert!(!step.wants_tools);
    }
}
