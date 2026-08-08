use serde_json::Value;

/// The stdout framing emitted by a non-interactive backend CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// Human-readable stdout. Chunks are passed through unchanged.
    Text,
    /// Claude Code `--output-format stream-json --include-partial-messages`.
    ClaudeJsonl,
    /// Codex `exec --json` events.
    CodexJsonl,
    /// Prime Agent `--mode json` session events.
    PrimeAgentJsonl,
}

/// A decoded stdout event has two independent consumers:
///
/// - `display` is streamed live to the terminal/dashboard and may contain progress.
/// - `answer` is the clean agent response retained for aggregators and callers.
///
/// Keeping these separate prevents tool-progress events from contaminating a later synthesis.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DecodedChunk {
    pub display: Option<String>,
    pub answer: Option<String>,
}

pub struct StreamDecoder {
    format: StreamFormat,
    emitted_answer: bool,
    answer_ends_with_newline: bool,
    display_ends_with_newline: bool,
    fallback_answer: Option<String>,
    seen_structured: bool,
    backend_error: Option<String>,
}

impl StreamDecoder {
    pub fn new(format: StreamFormat) -> Self {
        Self {
            format,
            emitted_answer: false,
            answer_ends_with_newline: false,
            display_ends_with_newline: true,
            fallback_answer: None,
            seen_structured: false,
            backend_error: None,
        }
    }

    /// A failure the backend reported in-stream while still exiting 0 (prime-agent's JSON
    /// mode does this for provider errors). The caller must turn it into a dispatch error —
    /// otherwise an auth failure or quota error masquerades as a successful answer.
    pub fn take_backend_error(&mut self) -> Option<String> {
        self.backend_error.take()
    }

    pub fn decode_line(&mut self, line: &str) -> DecodedChunk {
        if self.format == StreamFormat::Text {
            return self.answer(line.to_string());
        }

        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            // Before any structured event: compatibility with older CLIs and startup warnings
            // that write plain text even when a structured format was requested. After one: a
            // malformed line is a torn/corrupt event — surface it, but never as answer text
            // (a partially written tool_execution record could leak the redacted `args.code`).
            if self.seen_structured {
                return self.progress("unparsed", line);
            }
            return self.answer(line.to_string());
        };
        self.seen_structured = true;

        match self.format {
            StreamFormat::Text => unreachable!("handled above"),
            StreamFormat::ClaudeJsonl => self.decode_claude(&value),
            StreamFormat::CodexJsonl => self.decode_codex(&value),
            StreamFormat::PrimeAgentJsonl => self.decode_prime_agent(&value),
        }
    }

    /// Flush a final fallback response and make structured streams terminal-friendly.
    pub fn finish(&mut self) -> DecodedChunk {
        if self.format == StreamFormat::Text {
            return DecodedChunk::default();
        }

        if !self.emitted_answer
            && let Some(fallback) = self.fallback_answer.take()
            && !fallback.is_empty()
        {
            let text = ensure_trailing_newline(fallback);
            return self.answer(text);
        }

        if self.emitted_answer && !self.answer_ends_with_newline {
            self.answer_ends_with_newline = true;
            let display = if self.display_ends_with_newline {
                None
            } else {
                self.display_ends_with_newline = true;
                Some("\n".to_string())
            };
            return DecodedChunk {
                display,
                answer: Some("\n".to_string()),
            };
        }

        DecodedChunk::default()
    }

    fn decode_claude(&mut self, value: &Value) -> DecodedChunk {
        match value.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let event = &value["event"];
                match event.get("type").and_then(Value::as_str) {
                    Some("content_block_delta")
                        if event["delta"].get("type").and_then(Value::as_str)
                            == Some("text_delta") =>
                    {
                        event["delta"]
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| self.answer(text.to_string()))
                            .unwrap_or_default()
                    }
                    Some("content_block_start")
                        if event["content_block"].get("type").and_then(Value::as_str)
                            == Some("tool_use") =>
                    {
                        let name = event["content_block"]
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        self.progress("tool", name)
                    }
                    _ => DecodedChunk::default(),
                }
            }
            // Claude emits a complete assistant message as well as partial stream events. Keep it
            // only as a compatibility fallback so the final text is never duplicated.
            Some("assistant") => {
                if let Some(text) = claude_message_text(value) {
                    self.fallback_answer = Some(text);
                }
                DecodedChunk::default()
            }
            Some("result") => {
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    self.fallback_answer = Some(text.to_string());
                }
                DecodedChunk::default()
            }
            _ => DecodedChunk::default(),
        }
    }

    fn decode_codex(&mut self, value: &Value) -> DecodedChunk {
        match value.get("type").and_then(Value::as_str) {
            Some("item.completed")
                if value["item"].get("type").and_then(Value::as_str) == Some("agent_message") =>
            {
                value["item"]
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| self.discrete_answer(text))
                    .unwrap_or_default()
            }
            Some("item.started") => {
                let item = &value["item"];
                match item.get("type").and_then(Value::as_str) {
                    Some("command_execution") => {
                        // Do not persist command arguments in the dashboard log: generated shell
                        // commands can contain credentials or other sensitive values.
                        self.progress("command", "running")
                    }
                    Some("mcp_tool_call") => {
                        let tool = item
                            .get("tool")
                            .or_else(|| item.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("MCP tool");
                        self.progress("tool", tool)
                    }
                    Some("web_search") => self.progress("search", "running"),
                    _ => DecodedChunk::default(),
                }
            }
            Some("turn.failed") | Some("error") => {
                if let Some(message) = json_error_message(value) {
                    self.fallback_answer = Some(message);
                }
                DecodedChunk::default()
            }
            _ => DecodedChunk::default(),
        }
    }

    /// Prime Agent `--mode json`: one `AgentSessionEvent` per line.
    ///
    /// Assistant text arrives as `message_update` events whose `assistantMessageEvent` carries a
    /// `text_delta`. The enclosing `message.content` is CUMULATIVE (each update repeats the whole
    /// text so far), so the delta — not the message — is what streams, or every chunk would be
    /// re-emitted and the answer would grow quadratically.
    ///
    /// Reasoning (`thinking_delta`) is deliberately dropped from the answer: it is the model's
    /// scratchpad, and an aggregator must synthesize the response, not the deliberation.
    fn decode_prime_agent(&mut self, value: &Value) -> DecodedChunk {
        match value.get("type").and_then(Value::as_str) {
            Some("message_update") => {
                let event = &value["assistantMessageEvent"];
                match event.get("type").and_then(Value::as_str) {
                    Some("text_delta") => event
                        .get("delta")
                        .and_then(Value::as_str)
                        .map(|text| self.answer(text.to_string()))
                        .unwrap_or_default(),
                    _ => DecodedChunk::default(),
                }
            }
            // The tool NAME is progress; its `args` are not logged. prime-agent's one
            // model-facing tool is an IPython kernel, so `args.code` is generated Python/shell
            // that can carry credentials — the same reason codex's command arguments are elided.
            Some("tool_execution_start") => {
                let tool = value
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                self.progress("tool", tool)
            }
            // A completed assistant message repeats the full text. Kept only as a fallback for
            // the case where no delta was seen (a non-streaming provider, or a run that failed
            // before the first delta), so the final answer is never duplicated.
            Some("message_end") => {
                let message = &value["message"];
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    return DecodedChunk::default();
                }
                // prime-agent's JSON mode exits 0 even for a failed turn (stop-reason
                // handling lives in its text mode only), so the in-stream error is the ONLY
                // failure signal. Record it as an error, never as answer text — otherwise a
                // 401 or quota message becomes a "successful" answer that cascades and
                // capability learning would score as a win.
                let stop_reason = message.get("stopReason").and_then(Value::as_str);
                let failed = matches!(stop_reason, Some("error") | Some("aborted"));
                if let Some(error) = message.get("errorMessage").and_then(Value::as_str) {
                    self.backend_error = Some(error.to_string());
                    return self.progress("error", error);
                }
                if failed {
                    let reason = format!("request {}", stop_reason.unwrap_or("failed"));
                    self.backend_error = Some(reason.clone());
                    return self.progress("error", &reason);
                }
                if let Some(text) = prime_agent_message_text(message) {
                    self.fallback_answer = Some(text);
                }
                DecodedChunk::default()
            }
            _ => DecodedChunk::default(),
        }
    }

    fn answer(&mut self, text: String) -> DecodedChunk {
        if text.is_empty() {
            return DecodedChunk::default();
        }
        self.emitted_answer = true;
        self.answer_ends_with_newline = text.ends_with('\n');
        self.display_ends_with_newline = self.answer_ends_with_newline;
        DecodedChunk {
            display: Some(text.clone()),
            answer: Some(text),
        }
    }

    fn discrete_answer(&mut self, text: &str) -> DecodedChunk {
        let prefix = if self.emitted_answer && !self.answer_ends_with_newline {
            "\n"
        } else {
            ""
        };
        self.answer(format!("{prefix}{text}"))
    }

    fn progress(&mut self, kind: &str, detail: &str) -> DecodedChunk {
        let prefix = if self.display_ends_with_newline {
            ""
        } else {
            "\n"
        };
        let text = format!("{prefix}[{kind}] {}\n", single_line(detail));
        self.display_ends_with_newline = true;
        DecodedChunk {
            display: Some(text),
            answer: None,
        }
    }
}

fn claude_message_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

/// The plain text of a finished prime-agent assistant message: `text` blocks only, so a
/// `thinking` block never leaks into the answer an aggregator reads.
fn prime_agent_message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn json_error_message(value: &Value) -> Option<String> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error")?.get("message")?.as_str())
        .map(str::to_string)
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn single_line(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() > MAX_CHARS {
        flattened = flattened.chars().take(MAX_CHARS).collect();
        flattened.push('…');
    }
    flattened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_passed_through() {
        let mut decoder = StreamDecoder::new(StreamFormat::Text);
        assert_eq!(
            decoder.decode_line("hello\n"),
            DecodedChunk {
                display: Some("hello\n".into()),
                answer: Some("hello\n".into()),
            }
        );
        assert_eq!(decoder.finish(), DecodedChunk::default());
    }

    #[test]
    fn claude_emits_partial_text_without_duplicating_final_message() {
        let mut decoder = StreamDecoder::new(StreamFormat::ClaudeJsonl);
        let first = decoder.decode_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}"#,
        );
        let second = decoder.decode_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"lo"}}}"#,
        );
        let complete = decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#,
        );
        let result = decoder.decode_line(r#"{"type":"result","result":"Hello"}"#);

        assert_eq!(first.answer.as_deref(), Some("Hel"));
        assert_eq!(second.answer.as_deref(), Some("lo"));
        assert_eq!(complete, DecodedChunk::default());
        assert_eq!(result, DecodedChunk::default());
        assert_eq!(decoder.finish().answer.as_deref(), Some("\n"));
    }

    #[test]
    fn claude_uses_final_result_when_partial_events_are_absent() {
        let mut decoder = StreamDecoder::new(StreamFormat::ClaudeJsonl);
        decoder.decode_line(r#"{"type":"result","result":"fallback"}"#);
        let final_chunk = decoder.finish();
        assert_eq!(final_chunk.display.as_deref(), Some("fallback\n"));
        assert_eq!(final_chunk.answer.as_deref(), Some("fallback\n"));
    }

    #[test]
    fn codex_keeps_progress_out_of_the_collected_answer() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let command = decoder.decode_line(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"bash -lc ls"}}"#,
        );
        let message = decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}"#,
        );

        assert_eq!(command.display.as_deref(), Some("[command] running\n"));
        assert_eq!(command.answer, None);
        assert_eq!(message.answer.as_deref(), Some("Done"));
        assert_eq!(decoder.finish().answer.as_deref(), Some("\n"));
    }

    #[test]
    fn codex_separates_discrete_agent_messages() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let first = decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"First"}}"#,
        );
        let second = decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Second"}}"#,
        );
        assert_eq!(first.answer.as_deref(), Some("First"));
        assert_eq!(second.answer.as_deref(), Some("\nSecond"));
    }

    /// Captured from `prime-agent --mode json` (0.7.1, 2026-08-08). `message.content` is
    /// cumulative across updates, so only the delta may stream — asserting on it here is the
    /// regression guard against re-emitting the whole answer on every event.
    #[test]
    fn prime_agent_streams_deltas_not_the_cumulative_message() {
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        let start = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"text","text":"","index":0}]},"assistantMessageEvent":{"type":"text_start","contentIndex":0}}"#,
        );
        let first = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"text","text":"H","index":0}]},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"H"}}"#,
        );
        let second = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"text","text":"HELLO","index":0}]},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"ELLO"}}"#,
        );
        let end = decoder.decode_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"HELLO"}],"stopReason":"stop"}}"#,
        );

        assert_eq!(start, DecodedChunk::default());
        assert_eq!(first.answer.as_deref(), Some("H"));
        assert_eq!(second.answer.as_deref(), Some("ELLO"));
        // The completed message must NOT be replayed once deltas already carried it.
        assert_eq!(end, DecodedChunk::default());
        assert_eq!(decoder.finish().answer.as_deref(), Some("\n"));
    }

    /// Reasoning and tool arguments are progress or scratchpad, never answer: `thinking_delta`
    /// is dropped entirely and a tool call contributes only its name.
    #[test]
    fn prime_agent_keeps_thinking_and_tool_args_out_of_the_answer() {
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        let thinking = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"}]},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"plan"}}"#,
        );
        let tool = decoder.decode_line(
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"ipython","args":{"code":"export TOKEN=sekrit"}}"#,
        );
        let text = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[]},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Done"}}"#,
        );

        assert_eq!(thinking, DecodedChunk::default());
        assert_eq!(tool.display.as_deref(), Some("[tool] ipython\n"));
        assert_eq!(tool.answer, None);
        let shown = tool.display.unwrap();
        assert!(!shown.contains("sekrit"), "tool args must not be logged");
        assert_eq!(text.answer.as_deref(), Some("Done"));
    }

    /// No delta ever arrived (non-streaming provider, or a failed turn): the completed message
    /// — or its error — is the fallback, and `thinking` blocks stay out of it.
    #[test]
    fn prime_agent_falls_back_to_the_completed_message() {
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        decoder.decode_line(
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"ignored"}]}}"#,
        );
        decoder.decode_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"scratch"},{"type":"text","text":"fallback"}],"stopReason":"stop"}}"#,
        );
        let final_chunk = decoder.finish();
        assert_eq!(final_chunk.answer.as_deref(), Some("fallback\n"));

        // A provider failure surfaces in the display stream and as a backend error for the
        // dispatcher — never as answer text: prime-agent's JSON mode exits 0 for a failed
        // turn, so this in-stream signal is the only thing standing between a 401 and a
        // "successful" answer that cascades would consume and learning would score as a win.
        let mut failed = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        let chunk = failed.decode_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 Unauthorized"}}"#,
        );
        assert!(chunk.display.unwrap().contains("401 Unauthorized"));
        assert_eq!(failed.finish().answer, None);
        assert_eq!(
            failed.take_backend_error().as_deref(),
            Some("401 Unauthorized")
        );
    }

    #[test]
    fn prime_agent_failure_after_partial_text_is_still_an_error() {
        // Deltas arrived, then the stream failed: the truncated text must not be promoted
        // to a successful answer just because something was emitted first.
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        decoder.decode_line(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"partial"}}"#,
        );
        decoder.decode_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"partial"}],"stopReason":"aborted"}}"#,
        );
        decoder.finish();
        assert_eq!(
            decoder.take_backend_error().as_deref(),
            Some("request aborted")
        );
    }

    #[test]
    fn malformed_structured_lines_are_not_lost() {
        // Before any structured event: plain text is the answer (older CLIs, wrapper noise).
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let chunk = decoder.decode_line("warning from wrapper\n");
        assert_eq!(chunk.answer.as_deref(), Some("warning from wrapper\n"));

        // After one: a malformed line is displayed but never joins the answer — a torn
        // tool_execution record would otherwise leak its redacted args into the transcript.
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        decoder.decode_line(
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ok"}}"#,
        );
        let torn = decoder
            .decode_line(r#"{"type":"tool_execution_start","toolName":"ipython","args":{"code":"secret"#);
        assert_eq!(torn.answer, None);
        assert!(torn.display.unwrap().contains("unparsed"));
    }

    #[test]
    fn progress_is_flattened_and_bounded() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let detail = format!("first\n{}", "x".repeat(300));
        let chunk = decoder.progress("command", &detail);
        let display = chunk.display.unwrap();
        assert!(!display.trim_end().contains('\n'));
        assert!(display.contains('…'));
    }
}
