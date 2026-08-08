//! Stream-chunk → transcript-line assembly for the TUI (design §11.3).
//!
//! Worker `chunk` events arrive at arbitrary boundaries; the inline viewport inserts only
//! COMPLETE lines into the scrollback (`insert_before`), keeping the trailing partial
//! line visible in the live area until its newline arrives.

/// Accumulates streamed text; yields completed lines and exposes the live tail.
#[derive(Debug, Default)]
pub struct LineAssembler {
    partial: String,
}

impl LineAssembler {
    /// Feed a chunk; returns every line COMPLETED by it (without trailing newlines).
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        let mut done = Vec::new();
        for piece in chunk.split_inclusive('\n') {
            if let Some(stripped) = piece.strip_suffix('\n') {
                self.partial.push_str(stripped);
                done.push(std::mem::take(&mut self.partial));
            } else {
                self.partial.push_str(piece);
            }
        }
        done
    }

    /// The unfinished trailing line (rendered live in the viewport, not yet scrollback).
    pub fn tail(&self) -> &str {
        &self.partial
    }

    /// Flush whatever remains as a final line (turn end).
    pub fn finish(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.partial))
        }
    }
}

/// True for tool/progress lines like `[tool] Read` — rendered dimmed and foldable (§11.4
/// T3: tool-call folding = these lines are visually de-emphasized in the transcript).
pub fn is_progress_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('[')
        && t.split_once(']')
            .map(|(head, _)| {
                matches!(
                    head.trim_start_matches('['),
                    "tool" | "command" | "search" | "agentpit"
                )
            })
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_lines_across_chunk_boundaries() {
        let mut asm = LineAssembler::default();
        assert!(asm.push("hel").is_empty());
        assert_eq!(asm.tail(), "hel");
        assert_eq!(asm.push("lo\nwor"), vec!["hello".to_string()]);
        assert_eq!(asm.tail(), "wor");
        assert_eq!(asm.push("ld\n"), vec!["world".to_string()]);
        assert_eq!(asm.tail(), "");
        assert_eq!(asm.finish(), None);
    }

    #[test]
    fn multiple_lines_in_one_chunk_and_final_flush() {
        let mut asm = LineAssembler::default();
        assert_eq!(asm.push("a\nb\nc"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(asm.finish(), Some("c".to_string()));
        assert_eq!(asm.finish(), None);
    }

    #[test]
    fn progress_lines_are_detected_for_folding() {
        assert!(is_progress_line("[tool] Read"));
        assert!(is_progress_line("[command] running"));
        assert!(is_progress_line(
            "[agentpit] dropped a 2097154-byte output line"
        ));
        assert!(!is_progress_line("regular answer text"));
        assert!(!is_progress_line("[2026-08-08] a date, not a tool"));
    }
}
