use std::env;

use maki_config::{AgentConfig, CompactionBuffer};
use maki_providers::{
    ContentBlock, Message, Model, RequestOptions, Role, StreamResponse, TokenUsage,
};
use tracing::info;

use super::history::{
    History, UNAVAILABLE_RESULT, close_dangling_tool_calls, remove_orphaned_tool_results,
};
use super::run::estimate_message_tokens;
use super::streaming::stream_with_retry;
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender, TurnCompleteEvent};

pub(super) const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If the summary contains a todo list, restore it with todo_write and keep it updated. If you learned important project context during this session, consider saving it to memory before it's lost.";
const SUMMARY_REQUEST: &str = "What did we do so far?";
const SUMMARY_REQUEST_WITH_TAIL: &str = "What did we do earlier in this session? The most recent messages are kept in full after your summary, so summarize only what came before them.";
const IMAGE_PLACEHOLDER: &str = "[image]";
const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
const TOOL_INPUT_PLACEHOLDER: &str = "[arguments elided]";
const TEXT_ELISION: &str = "\n[... elided ...]\n";
const PREVIOUS_SUMMARY_TAG: &str = "previous-summary";
const KEEP_LAST_TOOL_RESULTS: usize = 3;
const MAX_TEXT_CHARS: usize = 2_000;
const MIDDLE_DROP_DIVISOR: u32 = 2;
const OVERFLOW_EXPECTED: &str = "the loop only ends early on an overflow error";

/// How much of the summarized span to throw away when the summarizer itself runs
/// out of context, from least to most destructive. Each level is applied to a
/// fresh copy of the span, so a level means "everything up to here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reduction {
    /// Images, thinking, and all but the newest tool results.
    Baseline,
    /// Every tool result, and the arguments of every tool call.
    ToolPayloads,
    /// Oversized text blocks, elided middle out.
    LongText,
    /// Whole turns from the middle, keeping the first turn and the newest ones.
    MiddleTurns,
}

const REDUCTIONS: [Reduction; 4] = [
    Reduction::Baseline,
    Reduction::ToolPayloads,
    Reduction::LongText,
    Reduction::MiddleTurns,
];

/// Never let the kept tail be big enough to trigger the next compaction on its
/// own, or two compactions in a row would make no progress.
pub fn keep_recent_tokens(config: &AgentConfig, model: &Model) -> u32 {
    let usable = model
        .context_window
        .saturating_sub(config.compaction_buffer.resolve(model.context_window));
    config
        .compaction_keep_recent
        .resolve(model.context_window)
        .min(usable / 2)
}

pub(super) async fn compact_history(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    keep_recent: u32,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();
    let mut span: Vec<Message> = history.as_slice().to_vec();
    remove_orphaned_tool_results(&mut span);

    let (span_start, previous_summary) = anchor(&span, keep_recent);
    let tail = span.split_off(span_start + find_cut_point(&span[span_start..], keep_recent));
    span.drain(..span_start);

    let empty_tools = serde_json::json!([]);
    let mut last_error = None;

    for reduction in REDUCTIONS {
        let mut request = span.clone();
        reduce(&mut request, reduction);
        close_dangling_tool_calls(&mut request, UNAVAILABLE_RESULT);
        request.push(Message::user(summarizer_request(
            previous_summary.as_deref(),
        )));

        match stream_with_retry(
            provider,
            model,
            &request,
            crate::prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
        )
        .await
        {
            Ok(response) => {
                if reduction != Reduction::Baseline {
                    info!(?reduction, "summarized span reduced to fit");
                }
                return Ok(finish_compact(
                    response,
                    history,
                    tail,
                    event_tx,
                    compact_start,
                    model,
                ));
            }
            Err(e) if e.is_context_overflow() => last_error = Some(e),
            Err(e) => return Err(e),
        }
    }

    Err(last_error.expect(OVERFLOW_EXPECTED))
}

fn finish_compact(
    response: StreamResponse,
    history: &mut History,
    tail: Vec<Message>,
    event_tx: &EventSender,
    compact_start: std::time::Instant,
    model: &Model,
) -> TokenUsage {
    let kept_messages = tail.len();
    let kept_tokens = estimate_message_tokens(&tail);

    let _ = event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        cost: model.cost_of(&response.usage, false),
        context_size: Some(response.usage.output + kept_tokens),
    })));

    let request = if kept_messages == 0 {
        SUMMARY_REQUEST
    } else {
        SUMMARY_REQUEST_WITH_TAIL
    };
    let mut new_history = vec![
        Message::user(request.into()),
        response.message.into_summary(),
    ];
    new_history.extend(tail);
    history.replace(new_history);
    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        kept_messages,
        kept_tokens,
        "compaction completed"
    );

    response.usage
}

pub async fn compact(
    provider: &dyn maki_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    keep_recent: u32,
    event_tx: &EventSender,
) -> Result<(), AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, keep_recent, event_tx, &cancel).await?;

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        stop_reason: None,
    })?;

    Ok(())
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, buffer: CompactionBuffer) -> bool {
    let usable = model
        .context_window
        .saturating_sub(buffer.resolve(model.context_window));
    usage.context_tokens() >= usable
}

/// Where the span to summarize begins, and the summary it has to be merged into.
///
/// A previous summary already covers everything before it, so summarizing it
/// again only lets detail erode a second time. Instead the span starts after it
/// and the old summary is handed to the summarizer as text to update, which
/// keeps information that no recent message mentions from being dropped.
///
/// Falls back to the whole history when the messages after the previous summary
/// all fit in the kept tail, since there would be nothing left to merge.
fn anchor(messages: &[Message], keep_recent: u32) -> (usize, Option<String>) {
    let found = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| message.summary_text().map(|text| (index + 1, text)));

    match found {
        Some((start, summary)) if find_cut_point(&messages[start..], keep_recent) > 0 => {
            (start, Some(summary.to_string()))
        }
        _ => (0, None),
    }
}

fn summarizer_request(previous_summary: Option<&str>) -> String {
    let (intro, previous) = match previous_summary {
        Some(summary) => (
            crate::prompt::COMPACTION_UPDATE,
            format!("<{PREVIOUS_SUMMARY_TAG}>\n{summary}\n</{PREVIOUS_SUMMARY_TAG}>\n\n"),
        ),
        None => (crate::prompt::COMPACTION_USER, String::new()),
    };
    format!("{previous}{intro}\n{}", crate::prompt::COMPACTION_TEMPLATE)
}

/// A user message that is not carrying tool results, so it opens a turn.
fn is_turn_start(message: &Message) -> bool {
    matches!(message.role, Role::User)
        && !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

fn turn_starts(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_turn_start(message))
        .map(|(index, _)| index)
        .collect()
}

/// The newest messages are worth more verbatim than any summary of them, so keep
/// as many whole turns as `budget` allows and summarize only what came before.
/// Cutting on a turn start is what makes the kept tail valid on its own: a tool
/// result can never end up separated from the call it answers.
///
/// Returns the index of the first kept message, or the length when the budget
/// does not even cover the newest turn.
fn find_cut_point(messages: &[Message], budget: u32) -> usize {
    let mut cut = messages.len();
    let mut tokens = 0;
    for index in (1..messages.len()).rev() {
        tokens += estimate_message_tokens(&messages[index..=index]);
        if tokens > budget {
            break;
        }
        if is_turn_start(&messages[index]) {
            cut = index;
        }
    }
    cut
}

fn reduce(messages: &mut Vec<Message>, level: Reduction) {
    strip_images(messages);
    strip_thinking(messages);
    if level >= Reduction::ToolPayloads {
        strip_tool_payloads(messages);
    } else {
        strip_old_tool_results(messages);
    }
    if level >= Reduction::LongText {
        elide_long_text(messages);
    }
    if level >= Reduction::MiddleTurns {
        drop_middle_turns(messages);
    }
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
        }
    }
}

fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
}

fn strip_old_tool_results(messages: &mut [Message]) {
    let total: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();

    let mut seen = 0;
    for msg in messages {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                if seen < total.saturating_sub(KEEP_LAST_TOOL_RESULTS) {
                    *content = TOOL_RESULT_PLACEHOLDER.into();
                }
                seen += 1;
            }
        }
    }
}

/// Tool call arguments are the other half of the bulk: a single write call can
/// carry a whole file. The ids stay, so call and result stay paired.
fn strip_tool_payloads(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            match block {
                ContentBlock::ToolResult { content, .. } => {
                    *content = TOOL_RESULT_PLACEHOLDER.into();
                }
                ContentBlock::ToolUse { input, .. } => *input = TOOL_INPUT_PLACEHOLDER.into(),
                _ => {}
            }
        }
    }
}

fn elide_long_text(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            let ContentBlock::Text { text } = block else {
                continue;
            };
            let chars = text.chars().count();
            if chars <= MAX_TEXT_CHARS {
                continue;
            }
            let keep = MAX_TEXT_CHARS / 2;
            let head: String = text.chars().take(keep).collect();
            let tail: String = text.chars().skip(chars - keep).collect();
            *text = format!("{head}{TEXT_ELISION}{tail}");
        }
    }
}

/// Last resort. The goal is stated in the first turn and the freshest work is in
/// the last ones, so halve the span from the middle rather than from the front,
/// which is what dropping the oldest rounds used to do.
fn drop_middle_turns(messages: &mut Vec<Message>) {
    let starts = turn_starts(messages);
    if starts.len() < 3 {
        return;
    }
    let first_turn_end = starts[1];
    let target = estimate_message_tokens(messages) / MIDDLE_DROP_DIVISOR;
    let head = estimate_message_tokens(&messages[..first_turn_end]);

    let mut keep_from = messages.len();
    for &start in starts[1..].iter().rev() {
        if head + estimate_message_tokens(&messages[start..]) > target {
            break;
        }
        keep_from = start;
    }
    if keep_from <= first_turn_end {
        return;
    }

    messages.drain(first_turn_end..keep_from);
    remove_orphaned_tool_results(messages);
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("MAKI_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use maki_providers::provider::{BoxFuture, Provider};
    use maki_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use maki_storage::id::SessionRef;
    use serde_json::Value;
    use test_case::test_case;

    use super::*;

    const NO_TAIL: u32 = 0;
    const BIG_BUDGET: u32 = 1_000_000;
    const EXPECTED_TEXT_BLOCK: &str = "expected a text block";

    fn turn(text: &str) -> Message {
        Message::user(text.into())
    }

    fn reply(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    #[track_caller]
    fn text_of(message: &Message) -> &str {
        message.first_text_content().expect(EXPECTED_TEXT_BLOCK)
    }

    struct MockProvider {
        responses: Mutex<Vec<Result<StreamResponse, AgentError>>>,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Result<StreamResponse, AgentError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.requests.lock().unwrap().push(messages.to_vec());
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                responses.remove(0)
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn small_context_model(context_window: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
        }
    }

    #[test]
    fn compact_replaces_history_with_summary() {
        smol::block_on(async {
            let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(MockProvider::new(
                vec![Ok(text_response(StopReason::EndTurn))],
            ));
            let model = default_model();
            let (raw_tx, _rx) = flume::unbounded();
            let mut history = History::new(vec![
                Message::user("first".into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "reply".into(),
                    }],
                    ..Default::default()
                },
            ]);

            compact(
                &*provider,
                &model,
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
            )
            .await
            .unwrap();

            let msgs = history.as_slice();
            assert_eq!(msgs.len(), 2);
            assert_eq!(text_of(&msgs[0]), SUMMARY_REQUEST);
            assert!(matches!(msgs[1].role, Role::Assistant));
        });
    }

    #[test]
    fn compact_preparation_removes_orphan_result_and_tool_image() {
        use std::sync::Arc;

        use maki_providers::{ImageMediaType, ImageSource};

        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let image = ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8=")),
            };
            let mut orphan = Message {
                role: Role::User,
                content: vec![tool_result("orphan"), image.clone()],
                ..Default::default()
            };
            orphan.content.push(ContentBlock::Text {
                text: "keep text".into(),
            });
            let chat_image = Message {
                role: Role::User,
                content: vec![image],
                ..Default::default()
            };
            let mut history = History::new(vec![orphan, chat_image]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            let request = &requests[0];
            assert!(
                !request
                    .iter()
                    .flat_map(|message| &message.content)
                    .any(|block| matches!(
                        block,
                        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                    ))
            );
            assert!(
                request.iter().flat_map(|message| &message.content).any(
                    |block| matches!(block, ContentBlock::Text { text } if text == "keep text")
                )
            );
            assert!(request.iter().flat_map(|message| &message.content).any(
                |block| matches!(block, ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
            ));
        });
    }

    #[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
    #[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
    #[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
    #[test_case(51_199,  0,       0,       0,      64_000,  false ; "small_window_below_scaled_threshold")]
    #[test_case(51_200,  0,       0,       0,      64_000,  true  ; "small_window_at_scaled_threshold")]
    fn overflow_detection(
        input: u32,
        cache_read: u32,
        cache_creation: u32,
        output: u32,
        ctx_window: u32,
        expected: bool,
    ) {
        let model = small_context_model(ctx_window);
        let usage = TokenUsage {
            input,
            output,
            cache_read,
            cache_creation,
        };
        assert_eq!(
            is_overflow(&usage, &model, AgentConfig::default().compaction_buffer),
            expected
        );
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 53_999, false ; "explicit_tokens_below")]
    #[test_case(CompactionBuffer::Tokens(10_000), 54_000, true  ; "explicit_tokens_honored")]
    #[test_case(CompactionBuffer::Percent(50),    32_000, true  ; "explicit_percent_at_threshold")]
    fn overflow_with_explicit_buffer(buffer: CompactionBuffer, input: u32, expected: bool) {
        let model = small_context_model(64_000);
        let usage = TokenUsage {
            input,
            ..Default::default()
        };
        assert_eq!(is_overflow(&usage, &model, buffer), expected);
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use maki_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_thinking_removes_thinking_blocks() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_old_tool_results_keeps_newest() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old result 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t4".into(),
                    content: "keep 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t5".into(),
                    content: "keep 3".into(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert_eq!(messages[0].content.len(), 6);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t1")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t2")
        );
        assert!(
            matches!(&messages[0].content[2], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 1" && tool_use_id == "t3")
        );
        assert!(
            matches!(&messages[0].content[3], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 2" && tool_use_id == "t4")
        );
        assert!(
            matches!(&messages[0].content[4], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 3" && tool_use_id == "t5")
        );
        assert!(
            matches!(&messages[0].content[5], ContentBlock::Text { text } if text == "keep me")
        );
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, "bash", serde_json::json!({}))],
            ..Default::default()
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "output".into(),
            is_error: false,
        }
    }

    #[track_caller]
    fn assert_tool_results_have_calls(messages: &[Message]) {
        for (index, message) in messages.iter().enumerate() {
            for block in &message.content {
                let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                    continue;
                };
                assert!(matches!(message.role, Role::User));
                assert!(index > 0);
                assert!(
                    messages[index - 1]
                        .tool_uses()
                        .any(|(id, _, _)| id == tool_use_id)
                );
            }
        }
    }

    #[test]
    fn compact_history_retries_with_reduced_tool_payloads() {
        smol::block_on(async {
            const TOOL_USE_ID: &str = "call_dMZDTpEfz2JxMvFbqFHua1Zy";

            let provider = MockProvider::new(vec![
                Err(AgentError::Api {
                    status: 413,
                    message: "prompt is too long".into(),
                }),
                Ok(text_response(StopReason::EndTurn)),
            ]);
            let mut history = History::new(vec![
                turn("request"),
                tool_use(TOOL_USE_ID),
                Message {
                    role: Role::User,
                    content: vec![tool_result(TOOL_USE_ID)],
                    ..Default::default()
                },
                turn("prompt"),
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_tool_results_have_calls(request);
            }
            assert!(requests[0].iter().flat_map(|message| &message.content).any(
                |block| matches!(block, ContentBlock::ToolResult { content, .. } if content == "output")
            ));
            assert!(requests[1].iter().flat_map(|message| &message.content).all(
                |block| !matches!(block, ContentBlock::ToolResult { content, .. } if content != TOOL_RESULT_PLACEHOLDER)
            ));
        });
    }

    #[test]
    fn compaction_keeps_observation_before_dependent_reply() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![
                Message::observation("[monitor] build failed".into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "I will fix it".into(),
                    }],
                    ..Default::default()
                },
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            assert!(requests[0][0].is_observation());
            assert!(matches!(requests[0][1].role, Role::Assistant));
        });
    }

    #[test]
    fn compact_keeps_recent_turns_verbatim() {
        smol::block_on(async {
            const RECENT: &str = "the newest request";
            const NEWEST_REPLY: &str = "newest reply";

            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![
                turn("old request"),
                reply("old reply"),
                turn(RECENT),
                reply(NEWEST_REPLY),
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                BIG_BUDGET,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            let msgs = history.as_slice();
            assert_eq!(msgs.len(), 4);
            assert_eq!(text_of(&msgs[0]), SUMMARY_REQUEST_WITH_TAIL);
            assert!(matches!(msgs[1].role, Role::Assistant));
            assert_eq!(text_of(&msgs[2]), RECENT);
            assert_eq!(text_of(&msgs[3]), NEWEST_REPLY);

            let requests = provider.requests.lock().unwrap();
            assert!(
                !requests[0]
                    .iter()
                    .any(|message| message.first_text_content() == Some(RECENT))
            );
        });
    }

    #[test]
    fn compact_marks_the_summary_so_the_next_one_can_find_it() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![turn("request"), reply("reply")]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            assert_eq!(history.as_slice()[1].summary_text(), Some("response"));
            assert!(history.as_slice()[0].summary_text().is_none());
        });
    }

    #[test]
    fn compact_updates_the_previous_summary_instead_of_resummarizing_it() {
        smol::block_on(async {
            const PREVIOUS: &str = "## Goal\nship the parser";
            const NEW_WORK: &str = "now fix the lexer";

            let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
            let mut history = History::new(vec![
                turn(SUMMARY_REQUEST),
                reply(PREVIOUS).into_summary(),
                turn(NEW_WORK),
                reply("done"),
            ]);
            let (raw_tx, _rx) = flume::unbounded();

            compact_history(
                &provider,
                &default_model(),
                &mut history,
                NO_TAIL,
                &EventSender::new(raw_tx, 0),
                &CancelToken::none(),
            )
            .await
            .unwrap();

            let requests = provider.requests.lock().unwrap();
            let sent = &requests[0];
            assert!(
                !sent
                    .iter()
                    .any(|message| message.first_text_content() == Some(PREVIOUS)),
                "the previous summary is not part of the span to summarize again"
            );
            assert_eq!(text_of(&sent[0]), NEW_WORK);

            let instructions = text_of(sent.last().unwrap());
            assert!(instructions.contains(PREVIOUS));
            assert!(instructions.contains(crate::prompt::COMPACTION_UPDATE));
        });
    }

    #[test]
    fn anchor_falls_back_when_the_previous_summary_is_the_whole_history() {
        let messages = vec![
            turn(SUMMARY_REQUEST),
            reply("## Goal\nship it").into_summary(),
        ];

        assert_eq!(anchor(&messages, BIG_BUDGET), (0, None));
    }

    #[test]
    fn anchor_picks_the_newest_summary() {
        const NEWEST: &str = "second summary";
        let messages = vec![
            reply("first summary").into_summary(),
            turn("work"),
            reply(NEWEST).into_summary(),
            turn("more work"),
            reply("more"),
        ];

        assert_eq!(anchor(&messages, NO_TAIL), (3, Some(NEWEST.to_string())));
    }

    #[test]
    fn summarizer_request_without_a_previous_summary_asks_for_a_fresh_one() {
        let request = summarizer_request(None);

        assert!(request.contains(crate::prompt::COMPACTION_USER));
        assert!(request.contains(crate::prompt::COMPACTION_TEMPLATE));
        assert!(!request.contains(PREVIOUS_SUMMARY_TAG));
    }

    #[test_case(NO_TAIL,    4 ; "no_budget_keeps_nothing_verbatim")]
    #[test_case(BIG_BUDGET, 2 ; "budget_reaches_the_oldest_turn_start")]
    fn find_cut_point_cuts_on_turn_starts(budget: u32, expected: usize) {
        let messages = vec![turn("first"), reply("a"), turn("second"), reply("b")];
        assert_eq!(find_cut_point(&messages, budget), expected);
    }

    #[test]
    fn find_cut_point_never_splits_a_tool_call_from_its_result() {
        let messages = vec![
            turn("first"),
            tool_use("t1"),
            Message {
                role: Role::User,
                content: vec![tool_result("t1")],
                ..Default::default()
            },
        ];
        assert_eq!(find_cut_point(&messages, BIG_BUDGET), messages.len());
    }

    #[test_case(CompactionBuffer::Percent(15), 9_600  ; "percent_of_window")]
    #[test_case(CompactionBuffer::Percent(90), 25_600 ; "capped_at_half_the_usable_window")]
    fn keep_recent_tokens_resolves(keep: CompactionBuffer, expected: u32) {
        let config = AgentConfig {
            compaction_keep_recent: keep,
            ..AgentConfig::default()
        };
        assert_eq!(
            keep_recent_tokens(&config, &small_context_model(64_000)),
            expected
        );
    }

    #[test]
    fn strip_tool_payloads_keeps_the_pairing_ids() {
        let mut messages = vec![
            tool_use("t1"),
            Message {
                role: Role::User,
                content: vec![tool_result("t1")],
                ..Default::default()
            },
        ];
        strip_tool_payloads(&mut messages);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolUse { input, .. } if input == &serde_json::json!(TOOL_INPUT_PLACEHOLDER))
        );
        assert!(
            matches!(&messages[1].content[0], ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == TOOL_RESULT_PLACEHOLDER)
        );
    }

    #[test]
    fn elide_long_text_keeps_both_ends() {
        let mut messages = vec![Message::user(format!(
            "{}{}",
            "a".repeat(MAX_TEXT_CHARS),
            "z".repeat(MAX_TEXT_CHARS)
        ))];
        elide_long_text(&mut messages);

        let text = text_of(&messages[0]);
        assert!(text.starts_with('a'));
        assert!(text.ends_with('z'));
        assert_eq!(
            text.chars().count(),
            MAX_TEXT_CHARS + TEXT_ELISION.chars().count()
        );
    }

    #[test]
    fn drop_middle_turns_keeps_the_goal_and_the_newest_work() {
        const GOAL: &str = "the goal";
        const NEWEST: &str = "newest";
        const FILLER_CHARS: usize = 4_000;

        let filler = "x".repeat(FILLER_CHARS);
        let mut messages = vec![
            turn(GOAL),
            reply("ack"),
            turn(&filler),
            reply(&filler),
            turn(&filler),
            reply(&filler),
            turn(NEWEST),
            reply("done"),
        ];

        drop_middle_turns(&mut messages);

        let kept: Vec<&str> = messages.iter().map(text_of).collect();
        assert_eq!(kept, vec![GOAL, "ack", NEWEST, "done"]);
    }
}
