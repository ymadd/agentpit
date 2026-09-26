//! A loop's per-step files (design §9.1) and how clients page through them.
//!
//! The runner serves these over `loop_read`; the dashboard bridge, which runs on the same
//! machine, reads them straight from disk so looking at a parked loop never wakes it. Both
//! go through [`read_page`], so a page means the same bytes either way.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::wire::LoopFile;

use super::is_safe_rel_path;

/// `loop_read` without `max_bytes`.
pub const READ_DEFAULT_BYTES: u64 = 64 * 1024;
/// The largest page one `loop_read` returns.
pub const READ_MAX_BYTES: u64 = 1024 * 1024;

/// The rendered prompt.
pub fn prompt_rel(step_id: &str) -> String {
    format!("prompts/{step_id}.md")
}

/// The live output as streamed.
pub fn output_log_rel(step_id: &str) -> String {
    format!("outputs/{step_id}.log")
}

/// The final answer.
pub fn answer_rel(step_id: &str) -> String {
    format!("outputs/{step_id}.md")
}

/// A check's combined stdout/stderr.
pub fn check_log_rel(step_id: &str) -> String {
    format!("checks/{step_id}.log")
}

/// The shape of [`super::step_id`]: a node id, then dot-separated parts of the same
/// alphabet. Anything else is refused before it is joined into a path.
fn is_step_id_shaped(s: &str) -> bool {
    s.len() <= 128
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        })
}

/// The loop-relative path of one step file, or `None` for an unknown kind or a step id
/// that could leave the loop directory.
pub fn step_file_rel(what: LoopFile, step_id: &str) -> Option<String> {
    if !is_step_id_shaped(step_id) {
        return None;
    }
    let rel = match what {
        LoopFile::Prompt => prompt_rel(step_id),
        LoopFile::Output => output_log_rel(step_id),
        LoopFile::Answer => answer_rel(step_id),
        LoopFile::CheckLog => check_log_rel(step_id),
        LoopFile::Unknown => return None,
    };
    is_safe_rel_path(&rel).then_some(rel)
}

/// Bytes `[offset, next_offset)` of a file that is `size` bytes long right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePage {
    pub offset: u64,
    pub next_offset: u64,
    pub size: u64,
    pub text: String,
}

/// Up to `max` bytes from `offset`, on character boundaries: the tail of a character cut
/// by `offset` is skipped, and one cut by `max` is left for the next page. A file that
/// does not exist yet reads as empty.
pub fn read_page(path: &Path, offset: u64, max: u64) -> std::io::Result<FilePage> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FilePage {
                offset,
                next_offset: offset,
                size: 0,
                text: String::new(),
            });
        }
        Err(e) => return Err(e),
    };
    let size = f.metadata()?.len();
    let mut start = offset.min(size);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    let lead = buf
        .iter()
        .take(3)
        .take_while(|b| (**b & 0xC0) == 0x80)
        .count();
    if start > 0 && lead > 0 {
        buf.drain(..lead);
        start += lead as u64;
    }
    if let Err(e) = std::str::from_utf8(&buf) {
        if e.error_len().is_none() && e.valid_up_to() > 0 && start + (buf.len() as u64) < size {
            buf.truncate(e.valid_up_to());
        }
    }
    Ok(FilePage {
        offset: start,
        next_offset: start + buf.len() as u64,
        size,
        text: String::from_utf8_lossy(&buf).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_files_stay_inside_the_loop_directory() {
        assert_eq!(
            step_file_rel(LoopFile::Output, "fix.a1").as_deref(),
            Some("outputs/fix.a1.log")
        );
        assert_eq!(
            step_file_rel(LoopFile::CheckLog, "implement.i1-2.a3").as_deref(),
            Some("checks/implement.i1-2.a3.log")
        );
        for bad in [
            "",
            "..",
            "a..b",
            ".a1",
            "../x",
            "a/b",
            "/etc/passwd",
            "a\\b",
            "x\0y",
            "Fix.a1",
        ] {
            assert!(
                step_file_rel(LoopFile::Prompt, bad).is_none(),
                "{bad:?} must be refused"
            );
        }
        assert!(step_file_rel(LoopFile::Unknown, "fix.a1").is_none());
    }

    #[test]
    fn pages_never_split_a_character_and_missing_files_read_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.log");
        let text = "修正しました。テストは通ります。";
        std::fs::write(&path, text).unwrap();
        let mut offset = 0;
        let mut joined = String::new();
        loop {
            let page = read_page(&path, offset, 7).unwrap();
            assert_eq!(page.size, text.len() as u64);
            if page.next_offset == page.offset {
                break;
            }
            joined.push_str(&page.text);
            offset = page.next_offset;
        }
        assert_eq!(joined, text);
        // Starting inside a character skips to the next one.
        let page = read_page(&path, 1, 64).unwrap();
        assert_eq!(page.offset, 3);
        assert!(text.ends_with(&page.text));

        let missing = read_page(&tmp.path().join("nope.log"), 5, 10).unwrap();
        assert_eq!(
            (missing.offset, missing.next_offset, missing.size),
            (5, 5, 0)
        );
    }
}
