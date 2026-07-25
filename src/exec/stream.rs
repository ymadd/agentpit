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
}

impl StreamDecoder {
    pub fn new(format: StreamFormat) -> Self {
        Self {
            format,
            emitted_answer: false,
            answer_ends_with_newline: false,
            display_ends_with_newline: true,
            fallback_answer: None,
        }
    }

    pub fn decode_line(&mut self, line: &str) -> DecodedChunk {
        if self.format == StreamFormat::Text {
            return self.answer(line.to_string());
        }

        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            // Preserve compatibility with older CLIs and startup warnings that may still write
            // plain text to stdout even when a structured format was requested.
            return self.answer(line.to_string());
        };

        match self.format {
            StreamFormat::Text => unreachable!("handled above"),
            StreamFormat::ClaudeJsonl => self.decode_claude(&value),
            StreamFormat::CodexJsonl => self.decode_codex(&value),
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

    #[test]
    fn malformed_structured_lines_are_not_lost() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let chunk = decoder.decode_line("warning from wrapper\n");
        assert_eq!(chunk.answer.as_deref(), Some("warning from wrapper\n"));
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
