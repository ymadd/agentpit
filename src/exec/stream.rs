use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
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
    backend_session_ref: Option<String>,
    /// Claude/Prime report tool results by id after their start event. The value is the
    /// already-redacted display label, so completion can identify the exact row safely.
    claude_tools: HashMap<String, String>,
    prime_tools: HashMap<String, String>,
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
            backend_session_ref: None,
            claude_tools: HashMap::new(),
            prime_tools: HashMap::new(),
        }
    }

    /// A failure the backend reported in-stream while still exiting 0 (prime-agent's JSON
    /// mode does this for provider errors). The caller must turn it into a dispatch error —
    /// otherwise an auth failure or quota error masquerades as a successful answer.
    pub fn take_backend_error(&mut self) -> Option<String> {
        self.backend_error.take()
    }

    /// The backend's own session/thread id captured from the stream, when the format carries
    /// one (claude: top-level `session_id` on every event; codex: `thread.started`'s
    /// `thread_id` — both verified against the real CLIs, 2026-08-08). Opaque: only the
    /// owning adapter can turn it back into resume flags. `None` for Text streams.
    pub fn backend_session_ref(&self) -> Option<&str> {
        self.backend_session_ref.as_deref()
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
        if self.backend_session_ref.is_none()
            && let Some(sid) = value.get("session_id").and_then(Value::as_str)
        {
            self.backend_session_ref = Some(sid.to_string());
        }
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
                        let block = &event["content_block"];
                        if let (Some(id), Some(name)) = (
                            block.get("id").and_then(Value::as_str),
                            block.get("name").and_then(Value::as_str),
                        ) {
                            self.claude_tools.insert(id.to_string(), name.to_string());
                        }
                        // Claude's stream start carries `{input:{}}`; wait for the complete
                        // assistant event so the first visible row can say what will run.
                        DecodedChunk::default()
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
                let details: Vec<String> = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .map(|block| {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let detail = tool_start_detail(name, &block["input"]);
                        let label = detail.strip_prefix("▶ ").unwrap_or(&detail).to_string();
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            self.claude_tools.insert(id.to_string(), label);
                        }
                        detail
                    })
                    .collect();
                self.progress_many("tool", details)
            }
            Some("user") => {
                let details: Vec<String> = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                    .map(|block| {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let label = self
                            .claude_tools
                            .remove(id)
                            .unwrap_or_else(|| "tool".to_string());
                        if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                            format!("✗ {label} — failed")
                        } else {
                            format!("✓ {label} — succeeded")
                        }
                    })
                    .collect();
                self.progress_many("tool", details)
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
        if self.backend_session_ref.is_none()
            && value.get("type").and_then(Value::as_str) == Some("thread.started")
            && let Some(tid) = value.get("thread_id").and_then(Value::as_str)
        {
            self.backend_session_ref = Some(tid.to_string());
        }
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
            Some("item.completed")
                if value["item"].get("type").and_then(Value::as_str)
                    == Some("command_execution") =>
            {
                let item = &value["item"];
                let failed = item.get("status").and_then(Value::as_str) == Some("failed")
                    || item
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .is_some_and(|c| c != 0);
                let (glyph, status) = if failed {
                    ("✗", "failed")
                } else {
                    ("✓", "succeeded")
                };
                let exit = item
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .map(|code| format!(" (exit {code})"))
                    .unwrap_or_default();
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .map(redact_secrets)
                    .map(|c| format!(" — {}", single_line(&c)))
                    .unwrap_or_default();
                self.progress(
                    "command",
                    &format!("{glyph} shell{command} — {status}{exit}"),
                )
            }
            Some("item.started") => {
                let item = &value["item"];
                match item.get("type").and_then(Value::as_str) {
                    Some("command_execution") => {
                        let command = item
                            .get("command")
                            .and_then(Value::as_str)
                            .map(redact_secrets)
                            .map(|c| format!(" — {}", single_line(&c)))
                            .unwrap_or_default();
                        self.progress("command", &format!("▶ shell{command}"))
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
            // Show a bounded, credential-redacted summary of what is being executed.
            // Opaque `[tool] Bash` rows made it impossible to audit a run; dumping raw args
            // would swing too far the other way because generated commands may contain keys.
            Some("tool_execution_start") => {
                let detail = prime_tool_start_detail(value);
                let label = detail.strip_prefix("▶ ").unwrap_or(&detail).to_string();
                if let Some(id) = value.get("toolCallId").and_then(Value::as_str) {
                    self.prime_tools.insert(id.to_string(), label);
                }
                self.progress("tool", &detail)
            }
            Some("tool_execution_end") => {
                let tool = value
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let label = value
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|id| self.prime_tools.remove(id))
                    .unwrap_or_else(|| tool.to_string());
                let failed = value.get("isError").and_then(Value::as_bool) == Some(true);
                let (glyph, status) = if failed {
                    ("✗", "failed")
                } else {
                    ("✓", "succeeded")
                };
                self.progress("tool", &format!("{glyph} {label} — {status}"))
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

    fn progress_many(&mut self, kind: &str, details: Vec<String>) -> DecodedChunk {
        let mut display = String::new();
        for detail in details {
            if let Some(text) = self.progress(kind, &detail).display {
                display.push_str(&text);
            }
        }
        DecodedChunk {
            display: (!display.is_empty()).then_some(display),
            answer: None,
        }
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

fn prime_tool_start_detail(value: &Value) -> String {
    let tool = value
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    tool_start_detail(tool, &value["args"])
}

fn tool_start_detail(tool: &str, args: &Value) -> String {
    let lower = tool.to_ascii_lowercase();

    let detail = if matches!(lower.as_str(), "bash" | "shell" | "ipython" | "python") {
        first_string_arg(args, &["command", "cmd", "code"]).map(|raw| {
            let without_cell_magic = raw
                .strip_prefix("%%bash")
                .map(str::trim_start)
                .unwrap_or(raw);
            redact_secrets(without_cell_magic)
        })
    } else if matches!(lower.as_str(), "read" | "write" | "edit") {
        first_string_arg(args, &["path", "file_path", "file", "filename"]).map(ToString::to_string)
    } else if lower.contains("search") || lower == "grep" {
        first_string_arg(args, &["query", "pattern", "path"]).map(ToString::to_string)
    } else {
        // Generic tools only expose known descriptive fields, never their full args object.
        first_string_arg(args, &["path", "query", "url", "name"]).map(ToString::to_string)
    };

    match detail.filter(|s| !s.trim().is_empty()) {
        Some(detail) => format!("▶ {tool} — {}", single_line(&detail)),
        None => format!("▶ {tool}"),
    }
}

fn first_string_arg<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| args.get(*name).and_then(Value::as_str))
}

fn redact_secrets(text: &str) -> String {
    // Covers environment assignments and common CLI options while leaving the command
    // structure visible. The transcript is durable, so err on the side of redaction.
    static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)\b(token|secret|password|passwd|api[_-]?key|authorization|cookie)\b(\s*=\s*)([^\s;]+)"#,
        )
        .expect("credential assignment regex")
    });
    static FLAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(--(?:token|secret|password|passwd|api[_-]?key|authorization|cookie)(?:=|\s+))([^\s;]+)"#,
        )
        .expect("credential flag regex")
    });
    let assigned = ASSIGNMENT.replace_all(text, "$1$2<redacted>");
    FLAG.replace_all(&assigned, "$1<redacted>").into_owned()
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
    fn claude_shows_tool_detail_and_result_without_adding_it_to_the_answer() {
        let mut decoder = StreamDecoder::new(StreamFormat::ClaudeJsonl);
        decoder.decode_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}}"#,
        );
        let start = decoder.decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"printf hello"}}]}}"#,
        );
        let end = decoder.decode_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"hello"}]}}"#,
        );

        assert_eq!(
            start.display.as_deref(),
            Some("[tool] ▶ Bash — printf hello\n")
        );
        assert_eq!(start.answer, None);
        assert_eq!(
            end.display.as_deref(),
            Some("[tool] ✓ Bash — printf hello — succeeded\n")
        );
        assert_eq!(end.answer, None);
    }

    #[test]
    fn codex_keeps_progress_out_of_the_collected_answer() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        let command = decoder.decode_line(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"bash -lc ls"}}"#,
        );
        let completed = decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"command_execution","command":"bash -lc ls","exit_code":0,"status":"completed"}}"#,
        );
        let message = decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}"#,
        );

        assert_eq!(
            command.display.as_deref(),
            Some("[command] ▶ shell — bash -lc ls\n")
        );
        assert_eq!(command.answer, None);
        assert_eq!(
            completed.display.as_deref(),
            Some("[command] ✓ shell — bash -lc ls — succeeded (exit 0)\n")
        );
        assert_eq!(completed.answer, None);
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
    fn prime_agent_shows_redacted_tool_detail_and_completion_status() {
        let mut decoder = StreamDecoder::new(StreamFormat::PrimeAgentJsonl);
        let thinking = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan"}]},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"plan"}}"#,
        );
        let tool = decoder.decode_line(
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"Bash","args":{"code":"%%bash\necho hello; export TOKEN=sekrit"}}"#,
        );
        let finished = decoder.decode_line(
            r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"Bash","result":{},"isError":false}"#,
        );
        let text = decoder.decode_line(
            r#"{"type":"message_update","message":{"role":"assistant","content":[]},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Done"}}"#,
        );

        assert_eq!(thinking, DecodedChunk::default());
        assert_eq!(tool.answer, None);
        let shown = tool.display.unwrap();
        assert!(
            shown.contains("echo hello"),
            "command summary is visible: {shown}"
        );
        assert!(
            shown.contains("<redacted>"),
            "credential value is redacted: {shown}"
        );
        assert!(!shown.contains("sekrit"), "credential must not be logged");
        assert_eq!(
            finished.display.as_deref(),
            Some("[tool] ✓ Bash — echo hello; export TOKEN=<redacted> — succeeded\n")
        );
        assert_eq!(finished.answer, None);
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
        let torn = decoder.decode_line(
            r#"{"type":"tool_execution_start","toolName":"ipython","args":{"code":"secret"#,
        );
        assert_eq!(torn.answer, None);
        assert!(torn.display.unwrap().contains("unparsed"));
    }

    #[test]
    fn claude_captures_session_id_from_any_event() {
        let mut decoder = StreamDecoder::new(StreamFormat::ClaudeJsonl);
        assert_eq!(decoder.backend_session_ref(), None);
        // Real shape (verified 2026-08-08): every claude event carries a top-level session_id.
        decoder.decode_line(
            r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"0c1ff28e-83d8-488c-86f7-a7213d9bd050","tools":[]}"#,
        );
        assert_eq!(
            decoder.backend_session_ref(),
            Some("0c1ff28e-83d8-488c-86f7-a7213d9bd050")
        );
        // First capture wins; a later event with another id does not overwrite it.
        decoder.decode_line(r#"{"type":"result","result":"ok","session_id":"other"}"#);
        assert_eq!(
            decoder.backend_session_ref(),
            Some("0c1ff28e-83d8-488c-86f7-a7213d9bd050")
        );
    }

    #[test]
    fn codex_captures_thread_id_from_thread_started() {
        let mut decoder = StreamDecoder::new(StreamFormat::CodexJsonl);
        // Real shape (verified 2026-08-08).
        decoder.decode_line(
            r#"{"type":"thread.started","thread_id":"019fe072-b0bc-7922-9d64-3e20a01c2805"}"#,
        );
        decoder.decode_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#,
        );
        assert_eq!(
            decoder.backend_session_ref(),
            Some("019fe072-b0bc-7922-9d64-3e20a01c2805")
        );
    }

    #[test]
    fn text_stream_has_no_session_ref() {
        let mut decoder = StreamDecoder::new(StreamFormat::Text);
        decoder.decode_line("session_id: not-a-structured-stream\n");
        assert_eq!(decoder.backend_session_ref(), None);
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
