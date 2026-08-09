//! `SKILL.md` discovery — the disk layer that feeds the slash registry's second layer.
//!
//! [`crate::cli::slash`] knows how to *hold* a runtime entry; this module is where one
//! comes from. A skill is a markdown file with YAML frontmatter, in either of the two
//! layouts a `.claude/skills/` directory uses in the wild:
//!
//! ```text
//! .claude/skills/critique/SKILL.md   ← directory layout (a skill with bundled files)
//! .claude/skills/critique.md         ← flat layout (what `agentpit init` writes)
//! ```
//!
//! …in either scope, project before user:
//!
//! ```text
//! <cwd>/.claude/skills   ← this project's skills
//! ~/.claude/skills       ← the ones that follow the user everywhere
//! ```
//!
//! Project wins a shared name because [`crate::cli::slash::Registry::resolve`] keeps the
//! first entry claiming a name and this module hands it the project scope first. A skill
//! that claims a BUILT-IN's name loses outright — that rule lives in `resolve`, not here,
//! so nothing on disk can take `/compact` away from the command the user already knows.
//!
//! Reading is eager: every body is loaded once, at startup, into the closure its registry
//! entry carries. A slash surface has to answer "what commands exist?" synchronously, and
//! a skill whose file vanished mid-session would otherwise be a row in the popup that
//! fails on Enter.
//!
//! ## What bounds the walk
//!
//! Everything here runs on the startup path, over directories this process does not own,
//! so each dimension is capped rather than trusted:
//!
//! * **Depth.** [`read_dir`](std::fs::read_dir) is called on each root and nowhere else. A
//!   directory entry is probed at exactly one path, `<dir>/SKILL.md`; its subdirectories
//!   are never descended into. Nothing recurses, so no symlink can make the walk loop.
//! * **Breadth.** At most [`MAX_ENTRIES_PER_ROOT`] entries per root, and at most the two
//!   roots [`roots`] returns.
//! * **Size.** A candidate larger than [`MAX_SKILL_BYTES`] is skipped, not truncated: its
//!   body is what every turn it composes carries, and half a skill is not a skill.
//! * **Rendered width.** A name is capped at [`NAME_WIDTH`] and a description at
//!   [`DESCRIPTION_WIDTH`], because both go straight into a `/help` row and a TUI popup
//!   column sized from the widest entry.
//!
//! ## What is skipped, and how a user finds out
//!
//! A bad entry never costs the user their other skills — but it is not swallowed either.
//! Every rejection is recorded as a [`Skipped`] with the reason, and [`skipped_note`]
//! renders the summary that `/help` prints under its command table.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::cli::slash::{Category, ExecKind, Form, ParseRule, SlashCommand, SlashSpec, Surface};

/// The file a directory-layout skill keeps its instructions in.
const SKILL_FILE: &str = "SKILL.md";

/// How much of a skill's `description` a one-line help row or popup entry carries.
const DESCRIPTION_WIDTH: usize = 72;

/// How long a slash name may be. A name comes from disk and sizes the help column, so a
/// pathological one would push every description off the right edge of the terminal.
const NAME_WIDTH: usize = 64;

/// How large a skill file may be. Its body rides in every turn the command composes, so
/// this is a token budget as much as a memory one.
const MAX_SKILL_BYTES: u64 = 64 * 1024;

/// How many directory entries one root may contribute. Past this the root is reported as
/// truncated rather than walked to the end of whatever happens to live there.
const MAX_ENTRIES_PER_ROOT: usize = 256;

/// How many skipped files [`skipped_note`] names before it falls back to a count.
const NOTE_ENTRIES: usize = 3;

/// How much of a path [`skipped_note`] shows. A file name is attacker-shaped input too.
pub(crate) const PATH_WIDTH: usize = 60;

/// One skill, as read off disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// The slash name, lowercased: the frontmatter `name`, or the file/directory stem.
    pub name: String,
    /// The one-line summary `/help` and the TUI popup show — sanitized and clipped.
    pub description: String,
    /// The `SKILL.md` (or `<name>.md`) itself — its directory is where bundled files live.
    pub path: PathBuf,
    /// Everything after the frontmatter: what a turn is composed from.
    pub instructions: String,
}

/// Why an entry under a skills root did not become a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A directory in a skills root with no `SKILL.md` inside it.
    NoSkillFile,
    /// Could not be read at all: permissions, a broken symlink, or not UTF-8.
    Unreadable,
    /// Bigger than [`MAX_SKILL_BYTES`].
    TooLarge,
    /// Frontmatter, but nothing under it to run.
    NoInstructions,
    /// A name no one could type at a slash prompt, or longer than [`NAME_WIDTH`].
    UnusableName,
    /// The root held more than [`MAX_ENTRIES_PER_ROOT`] entries; the rest were not read.
    TooManyEntries,
}

impl SkipReason {
    /// The phrase a user sees after the path.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NoSkillFile => "no SKILL.md inside",
            SkipReason::Unreadable => "unreadable",
            SkipReason::TooLarge => "too large",
            SkipReason::NoInstructions => "no instructions under the frontmatter",
            SkipReason::UnusableName => "unusable name",
            SkipReason::TooManyEntries => "too many entries; the rest were not read",
        }
    }
}

/// One entry that did not become a command, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    /// The file (or, for [`SkipReason::TooManyEntries`], the root) that was passed over.
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// What one pass over the roots produced: the commands, and everything it refused.
///
/// Not `Debug`: a spec carries a closure, which is why [`crate::cli::slash::Registry`]
/// prints counts instead of rows.
#[derive(Default)]
pub struct Discovery {
    pub specs: Vec<SlashSpec>,
    pub skipped: Vec<Skipped>,
}

/// What the process's one [`discover`] call refused, for the surfaces that report it.
static SKIPPED: OnceLock<Vec<Skipped>> = OnceLock::new();

/// The registry entries `cwd`'s project scope and the user scope declare, project first.
///
/// The skipped list is recorded process-wide rather than returned, mirroring
/// [`crate::cli::slash::install`]: discovery happens once, at the entry point that knows
/// the session's cwd, and `/help` reads the outcome later from somewhere else entirely.
/// Tests drive [`discover_in`], which returns both halves and touches no global.
pub fn discover(cwd: &Path) -> Vec<SlashSpec> {
    let found = discover_in(&roots(cwd));
    let _ = SKIPPED.set(found.skipped);
    found.specs
}

/// What [`discover`] refused, or an empty slice in a process that never called it.
pub fn skipped() -> &'static [Skipped] {
    SKIPPED.get().map(Vec::as_slice).unwrap_or(&[])
}

/// The `.claude/skills` directories to read, in precedence order. A missing one is not an
/// error — most projects have no project scope, and a fresh machine has no user scope.
pub fn roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".claude").join("skills")];
    if let Some(user) = dirs::home_dir().map(|h| h.join(".claude").join("skills"))
        && !roots.contains(&user)
    {
        roots.push(user);
    }
    roots
}

/// [`discover`] over explicit roots — the seam the tests drive with a temp directory
/// instead of the machine's own `.claude/`.
pub fn discover_in(roots: &[PathBuf]) -> Discovery {
    let mut found = Discovery::default();
    for root in roots {
        let (skills, mut skipped) = read_root(root);
        found.specs.extend(skills.into_iter().map(spec));
        found.skipped.append(&mut skipped);
    }
    found
}

/// The one-line summary `/help` prints under its command table, or `None` when discovery
/// refused nothing. Paths are sanitized and clipped like any other rendered string: a file
/// name is as much outside input as the frontmatter inside it.
pub fn skipped_note(skipped: &[Skipped]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let named: Vec<String> = skipped
        .iter()
        .take(NOTE_ENTRIES)
        .map(|s| {
            format!(
                "{} ({})",
                render_safe(&s.path.display().to_string(), PATH_WIDTH),
                s.reason.as_str()
            )
        })
        .collect();
    let rest = skipped.len() - named.len();
    let more = if rest > 0 {
        format!(", and {rest} more")
    } else {
        String::new()
    };
    let noun = if skipped.len() == 1 {
        "entry"
    } else {
        "entries"
    };
    Some(format!(
        "Skipped {} skill {noun}: {}{more}",
        skipped.len(),
        named.join(", ")
    ))
}

/// What one directory entry turned out to be.
enum Read {
    /// A skill, ready to become a command.
    Skill(Box<Skill>),
    /// A skill that could not be used, and why — reported, not swallowed.
    Skipped(SkipReason),
    /// Not a skill at all (a `.DS_Store`, a `notes.txt`): nothing to report.
    NotACandidate,
}

/// Every skill one root declares, sorted by path so two runs on one machine agree — which
/// is also the order a name shared inside one scope is settled in.
///
/// A root that is simply absent is silent; one that exists and cannot be read is reported,
/// since that is a permission problem the user can fix.
fn read_root(root: &Path) -> (Vec<Skill>, Vec<Skipped>) {
    let mut skipped = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                skipped.push(Skipped {
                    path: root.to_path_buf(),
                    reason: SkipReason::Unreadable,
                });
            }
            return (Vec::new(), skipped);
        }
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    if paths.len() > MAX_ENTRIES_PER_ROOT {
        skipped.push(Skipped {
            path: root.to_path_buf(),
            reason: SkipReason::TooManyEntries,
        });
        paths.truncate(MAX_ENTRIES_PER_ROOT);
    }
    let mut skills = Vec::new();
    for path in &paths {
        match read_skill(path) {
            Read::Skill(skill) => skills.push(*skill),
            Read::Skipped(reason) => skipped.push(Skipped {
                path: path.clone(),
                reason,
            }),
            Read::NotACandidate => {}
        }
    }
    (skills, skipped)
}

/// Read one directory entry as a skill, in whichever layout it is written.
///
/// Every failure is a value, not a panic and not a silent `None`: a skills root is outside
/// input, and one bad file must cost the user that file alone.
fn read_skill(entry: &Path) -> Read {
    let (file, stem) = if entry.is_dir() {
        let file = entry.join(SKILL_FILE);
        if !file.is_file() {
            return Read::Skipped(SkipReason::NoSkillFile);
        }
        match entry.file_name() {
            Some(stem) => (file, stem.to_os_string()),
            None => return Read::NotACandidate,
        }
    } else if entry
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
    {
        match entry.file_stem() {
            Some(stem) => (entry.to_path_buf(), stem.to_os_string()),
            None => return Read::NotACandidate,
        }
    } else {
        return Read::NotACandidate;
    };

    // Size before content: the body rides in every turn this command composes, so an
    // oversized file is refused rather than read and clipped into half a skill.
    match std::fs::metadata(&file) {
        Ok(meta) if meta.len() > MAX_SKILL_BYTES => return Read::Skipped(SkipReason::TooLarge),
        Ok(_) => {}
        Err(_) => return Read::Skipped(SkipReason::Unreadable),
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Read::Skipped(SkipReason::Unreadable);
    };

    let (front, body) = split_frontmatter(&text);
    let instructions = body.trim();
    if instructions.is_empty() {
        return Read::Skipped(SkipReason::NoInstructions);
    }
    let declared = front_value(front, "name");
    let raw_name = match declared.as_deref().or_else(|| stem.to_str()) {
        Some(raw) => raw,
        // A file name that is not UTF-8 cannot be typed at a prompt either.
        None => return Read::Skipped(SkipReason::UnusableName),
    };
    let Some(name) = command_name(raw_name) else {
        return Read::Skipped(SkipReason::UnusableName);
    };
    let description = front_value(front, "description")
        .map(|d| render_safe(&d, DESCRIPTION_WIDTH))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("Run the {name} skill"));
    Read::Skill(Box::new(Skill {
        name,
        description,
        path: file,
        instructions: instructions.to_string(),
    }))
}

/// Split `---`-delimited YAML frontmatter from the body. A file without it is all body.
fn split_frontmatter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---") else {
        return ("", text);
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    match rest.split_once("\n---") {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\r', '\n'])),
        // A fence that never closes is a truncated file, not frontmatter: keep the whole
        // text as the body rather than losing the instructions to a half-written header.
        None => ("", text),
    }
}

/// One top-level scalar out of the frontmatter, e.g. `name: critique`.
///
/// Deliberately not a YAML parser: the two keys a slash command needs are single-line
/// scalars, and taking on a YAML dependency to read them would buy nothing. A key that is
/// nested or continued is simply not found, and the caller falls back.
///
/// The one YAML shape worth understanding is the **block scalar** — `description: |` (or
/// `>`) with the text on the following indented lines. A line parser that stops at the
/// colon reads the indicator itself as the value, and authors reach for the block form
/// precisely when the description is long, so the row that most needs text renders as a
/// bare `|`. Dogfooding, 2026-08-09: `~/.claude/skills/ja-doc-craft/SKILL.md` did exactly
/// that. The block's first non-empty line is taken, matching the one-row policy every
/// other multi-line description already gets.
fn front_value(front: &str, key: &str) -> Option<String> {
    /// `|`, `>`, and their chomping/indentation indicators (`|-`, `>+`, `|2`).
    fn block_indicator(rest: &str) -> bool {
        let mut chars = rest.chars();
        matches!(chars.next(), Some('|' | '>'))
            && chars.all(|c| matches!(c, '-' | '+') || c.is_ascii_digit())
    }

    let lines: Vec<&str> = front.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let rest = rest.trim();
        if block_indicator(rest) {
            // The block runs while lines stay indented; blank lines inside it are skipped
            // rather than ending it, so an author's paragraph break costs nothing.
            let folded = lines[i + 1..]
                .iter()
                .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
                .map(|l| l.trim())
                .find(|l| !l.is_empty());
            if let Some(value) = folded {
                return Some(value.to_string());
            }
            continue;
        }
        let rest = rest
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .or_else(|| rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .unwrap_or(rest);
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
    }
    None
}

/// The slash name a raw frontmatter/file name becomes, or `None` when it cannot be one.
///
/// Lowercased because every surface filters on a lowercased prefix (the REPL completer and
/// the TUI popup both do), so an upper-case row would be listed and never match what the
/// user types. Anything outside `[a-z0-9._-]` is refused rather than mangled: a name with
/// a space in it would be unreachable, since `parse` splits the line at the first
/// whitespace — and an escape sequence in it would reach the terminal as an escape
/// sequence. The character class doing double duty as the sanitizer is why a name needs no
/// separate [`render_safe`] pass; the length cap is here for the same reason it is there.
fn command_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= NAME_WIDTH
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then_some(name)
}

/// A string from disk, made safe to print and clipped to `width` — the one gate every
/// rendered skill string goes through.
///
/// Terminal surfaces write these straight out (the REPL `println!`s its help; ratatui
/// writes a cell's symbol verbatim), so an escape sequence in a `description:` would
/// otherwise repaint the screen, move the cursor, or set a title. Dropped, in order:
///
/// * everything after the first line — both help surfaces render one row per command;
/// * complete ANSI escape sequences, CSI and OSC alike, so no `[31m` residue is left
///   behind by removing the ESC alone;
/// * every other control character (C0 and C1), collapsed into the surrounding spacing;
/// * the Unicode bidi overrides, which reorder a row's text against its bytes.
///
/// What survives is clipped on a char boundary to `width`, ellipsis included.
///
/// `pub(crate)` because it is the gate for the OTHER runtime layer too: an MCP prompt's
/// name and description come off a wire rather than off a disk, land in the same `/help`
/// table and the same popup column, and must therefore go through this exact pass rather
/// than a second one written next to them ([`crate::mcp::prompts`]).
pub(crate) fn render_safe(text: &str, width: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: parameters, then one final byte in 0x40..=0x7E.
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: a string terminated by BEL or ST.
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Any other two-character escape.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // Bidi overrides and isolates: they reorder what is drawn, so a row can read
            // as something other than what it says.
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    // Collapse the runs the substitutions above can leave, so the help column stays a
    // column.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= width {
        return collapsed;
    }
    let clipped: String = collapsed.chars().take(width.saturating_sub(1)).collect();
    format!("{}…", clipped.trim_end())
}

/// The registry row one skill becomes.
///
/// `ExecKind::Dispatch` because running it spends tokens, and only the two surfaces a user
/// types into: a skill has nothing to offer the `attach` client, whose worker serves a
/// fixed set of verbs.
fn spec(skill: Skill) -> SlashSpec {
    let Skill {
        name,
        description,
        path,
        instructions,
    } = skill;
    let dir = path.parent().unwrap_or(&path).to_path_buf();
    let command_name = name.clone();
    SlashSpec {
        name: Cow::Owned(name),
        aliases: Cow::Owned(Vec::new()),
        forms: Cow::Owned(vec![Form {
            args: Cow::Borrowed("[text]"),
            description: Cow::Owned(description),
        }]),
        category: Category::Agents,
        surfaces: Cow::Owned(vec![Surface::Repl, Surface::Tui]),
        exec: ExecKind::Dispatch,
        // The closure is the whole reason `ParseRule` has a second shape: it carries the
        // instructions and the file this entry was read from, which a `fn` pointer in a
        // `static` table cannot.
        parse: ParseRule::owned(move |rest| {
            let prompt = compose(&command_name, &dir, &instructions, rest);
            Ok(SlashCommand::Skill {
                name: command_name.clone(),
                provenance: provenance(&command_name, &path, &prompt),
                prompt,
            })
        }),
    }
}

/// The turn a skill invocation becomes: its instructions, where its files are, and — when
/// the user typed one — the request to apply them to.
///
/// Pure, and the only place the layout is decided: the surfaces hand the result to the
/// dispatch they already had, so what a skill *is* cannot differ between the REPL and the
/// TUI. The order is fixed — header, then instructions, then the user's text last — so the
/// request is the nearest thing to the reply and cannot be buried mid-body by a long file.
///
/// **A skill invoked with no arguments still runs.** It composes to the instructions alone,
/// with no request section appended, which is why the argument hint is `[text]` and not
/// `<text>`: most skills read the conversation they are invoked in ("write the postmortem",
/// "critique the plan above"), and refusing a bare `/postmortem` would make the common case
/// the error case. An empty or all-whitespace argument is the same thing as none.
///
/// Nothing here slices, so every boundary is a char boundary: `arg` is trimmed and appended
/// whole, however many bytes each of its characters happens to take.
pub fn compose(name: &str, dir: &Path, instructions: &str, arg: &str) -> String {
    let mut turn = format!(
        "Follow the skill \"{name}\" for this turn. Its files are in {}.\n\n{instructions}",
        dir.display()
    );
    let arg = arg.trim();
    if !arg.is_empty() {
        turn.push_str(&format!("\n\n---\n\nThe user's request: {arg}"));
    }
    turn
}

/// The line a surface shows BEFORE it sends a composed turn: which skill, how much text it
/// adds, and which file that text came out of.
///
/// A skill turn is the one place where a keystroke sends kilobytes the user never typed, so
/// naming the command alone would not be provenance — the size and the source path are what
/// make an unexpected `/critique` legible after the fact. Both surfaces print this verbatim
/// to a terminal, so the path goes through [`render_safe`] exactly like a skipped entry's
/// does; the name needs no pass, having already been through [`command_name`].
pub fn provenance(name: &str, path: &Path, prompt: &str) -> String {
    format!(
        "[skill /{name} — {} from {}]",
        size_label(prompt.len()),
        render_path(path, PATH_WIDTH)
    )
}

/// A path, sanitized like any other rendered string but shortened from the FRONT.
///
/// What identifies a skill is the end of its path (`…/skills/critique/SKILL.md`); the head
/// is whatever `$HOME` and the checkout happen to be. Clipping the tail the way
/// [`skipped_note`] does — which is right there, where the head is the news — would leave
/// this line naming no file at all on a machine with a deep working directory.
///
/// `pub(crate)` for the same reason [`render_safe`] is: an MCP server's origin is a path
/// too, and its provenance line has the same head/tail problem.
pub(crate) fn render_path(path: &Path, width: usize) -> String {
    let safe = render_safe(&path.display().to_string(), usize::MAX);
    let count = safe.chars().count();
    if count <= width {
        return safe;
    }
    let tail: String = safe.chars().skip(count - width.saturating_sub(1)).collect();
    format!("…{tail}")
}

/// A byte count as a reader would say it: whole bytes below a kibibyte, one decimal above.
///
/// Shared with [`crate::mcp::prompts`] so the two composed-turn layers report a size the
/// same way: a provenance line is read to compare what a keystroke just sent, and two
/// spellings of "how big" would make that comparison a conversion.
pub(crate) fn size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

// ─── test fixtures ───────────────────────────────────────────────────────────────
// Shared with the surface tests (`tui::completion`, `tui::slash`), so what they prove is
// "a real SKILL.md reaches this surface", not "a hand-built struct does".

/// Which layout [`write_skill`] writes.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum Layout {
    /// `<root>/<name>/SKILL.md`
    Directory,
    /// `<root>/<name>.md`
    Flat,
}

/// The body every fixture skill carries.
#[cfg(test)]
pub(crate) const TEST_BODY: &str = "Ask the reviewer for three concrete objections.";

/// Write a skill file under `root` in the given layout.
#[cfg(test)]
pub(crate) fn write_skill(root: &Path, name: &str, layout: Layout, front: &str, body: &str) {
    let file = match layout {
        Layout::Directory => {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("skill dir");
            dir.join(SKILL_FILE)
        }
        Layout::Flat => {
            std::fs::create_dir_all(root).expect("skills dir");
            root.join(format!("{name}.md"))
        }
    };
    std::fs::write(file, format!("---\n{front}\n---\n\n{body}\n")).expect("write skill");
}

/// The fixture skill with a name no built-in could have: what a help or popup column sized
/// for the built-in table alone would run into.
#[cfg(test)]
pub(crate) const TEST_LONG_NAME: &str = "a-very-long-discovered-skill-name";

/// A resolved registry carrying two skills read off disk — `/critique` in the directory
/// layout, and [`TEST_LONG_NAME`] in the flat one.
///
/// The temp directory is gone by the time this returns: bodies are read eagerly, so the
/// registry does not outlive-borrow anything on disk.
#[cfg(test)]
pub(crate) fn test_registry_from_disk() -> &'static crate::cli::slash::Registry {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join(".claude/skills");
    write_skill(
        &root,
        "critique",
        Layout::Directory,
        "name: critique\ndescription: Argue against the current plan",
        TEST_BODY,
    );
    write_skill(
        &root,
        TEST_LONG_NAME,
        Layout::Flat,
        &format!("name: {TEST_LONG_NAME}\ndescription: Named longer than any built-in"),
        TEST_BODY,
    );
    crate::cli::slash::test_registry(discover_in(&[root]).specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::slash::{Parsed, Registry, names_for, test_registry};

    /// A project root and a user root, one skill each, in opposite layouts — the "either
    /// layout, either scope" fixture.
    fn two_scopes(dir: &Path) -> Vec<PathBuf> {
        let project = dir.join("project/.claude/skills");
        let user = dir.join("user/.claude/skills");
        write_skill(
            &project,
            "critique",
            Layout::Directory,
            "name: critique\ndescription: Argue against the current plan",
            TEST_BODY,
        );
        write_skill(
            &user,
            "postmortem",
            Layout::Flat,
            "name: postmortem\ndescription: Write the blameless writeup",
            "Cover timeline, impact, and the fix.",
        );
        vec![project, user]
    }

    /// The names one root declares, for the tests that only care that it parsed.
    fn names_in(root: &Path) -> Vec<String> {
        read_root(root).0.into_iter().map(|s| s.name).collect()
    }

    // ─── reading one file ────────────────────────────────────────────────────────

    #[test]
    fn both_layouts_read_the_same_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(
            root,
            "nested",
            Layout::Directory,
            "name: nested\ndescription: A skill with bundled files",
            TEST_BODY,
        );
        write_skill(
            root,
            "flat",
            Layout::Flat,
            "name: flat\ndescription: A single-file skill",
            TEST_BODY,
        );
        let (found, skipped) = read_root(root);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["flat", "nested"], "both layouts, sorted");
        assert!(skipped.is_empty(), "{skipped:?}");
        for skill in &found {
            assert_eq!(skill.instructions, TEST_BODY);
        }
        // The directory layout points at the file, so its parent is where bundled files
        // live — that path is what the composed turn tells the agent to look in.
        let nested = found.iter().find(|s| s.name == "nested").unwrap();
        assert_eq!(nested.path, root.join("nested").join(SKILL_FILE));
        assert_eq!(nested.description, "A skill with bundled files");
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("nope");
        // Absent is the normal case (most projects have no project scope), so it is not
        // even reported: a note naming every project without one would be noise.
        assert_eq!(read_root(&absent).0.len(), 0);
        assert!(read_root(&absent).1.is_empty());
        let found = discover_in(&[absent]);
        assert!(found.specs.is_empty());
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn entries_that_cannot_be_a_command_are_skipped_not_guessed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A directory with no SKILL.md in it…
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();
        // …a file that is not markdown…
        std::fs::write(root.join("notes.txt"), "---\nname: notes\n---\nbody").unwrap();
        // …a skill with frontmatter but nothing to run…
        write_skill(root, "hollow", Layout::Flat, "name: hollow", "");
        // …and a name no one could type, because `parse` splits at the first space.
        write_skill(
            root,
            "spaced",
            Layout::Flat,
            "name: two words\ndescription: unreachable",
            TEST_BODY,
        );
        let (skills, skipped) = read_root(root);
        assert!(skills.is_empty());
        // Each rejection is reported with its reason — except the .txt, which was never a
        // skill candidate and would only be noise in the note.
        let reported: Vec<(String, SkipReason)> = skipped
            .iter()
            .map(|s| {
                (
                    s.path.file_name().unwrap().to_string_lossy().into_owned(),
                    s.reason,
                )
            })
            .collect();
        assert_eq!(
            reported,
            vec![
                ("empty-dir".to_string(), SkipReason::NoSkillFile),
                ("hollow.md".to_string(), SkipReason::NoInstructions),
                ("spaced.md".to_string(), SkipReason::UnusableName),
            ]
        );
    }

    #[test]
    fn the_stem_names_a_skill_whose_frontmatter_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(
            root,
            "Fallback",
            Layout::Flat,
            "description: No name key",
            TEST_BODY,
        );
        let found = read_root(root).0;
        // Lowercased: every surface filters on a lowercased prefix, so an upper-case row
        // would be listed and never match what the user types.
        assert_eq!(found[0].name, "fallback");
        assert_eq!(found[0].description, "No name key");
    }

    #[test]
    fn frontmatter_reads_the_two_scalars_and_ignores_the_rest() {
        assert_eq!(
            front_value("name: a\ndescription: b", "name").as_deref(),
            Some("a")
        );
        assert_eq!(
            front_value("name: \"quoted name\"", "name").as_deref(),
            Some("quoted name")
        );
        assert_eq!(
            front_value("name: 'single'", "name").as_deref(),
            Some("single")
        );
        // Absent, empty, and nested keys all fall back rather than yielding a blank name.
        assert_eq!(front_value("description: b", "name"), None);
        assert_eq!(front_value("name:", "name"), None);
        assert_eq!(front_value("meta:\n  name: nested", "name"), None);
        // A body with no frontmatter at all is all body…
        assert_eq!(split_frontmatter("# Title\nbody"), ("", "# Title\nbody"));
        // …and so is one whose fence never closes.
        assert_eq!(split_frontmatter("---\nname: x").1, "---\nname: x");
    }

    /// A block scalar carries the description on the following indented lines. Reading the
    /// `|` as the value is the naive line-parser bug this guards: it renders the row as a
    /// bare pipe, and it bites exactly the long descriptions that need the row most
    /// (dogfooding, 2026-08-09: `~/.claude/skills/ja-doc-craft/SKILL.md`).
    #[test]
    fn a_block_scalar_description_reads_its_text_not_its_indicator() {
        let block = "name: ja-doc-craft\ndescription: |\n  日本語のビジネス文書を作るためのスキル。\n  次のときに読む: スライドを直すとき。\n";
        assert_eq!(
            front_value(block, "description").as_deref(),
            Some("日本語のビジネス文書を作るためのスキル。")
        );
        // The chomping and indentation indicators are part of the marker, not the text.
        for marker in ["|", "|-", "|+", ">", ">-", ">2"] {
            assert_eq!(
                front_value(
                    &format!("description: {marker}\n  folded text"),
                    "description"
                )
                .as_deref(),
                Some("folded text"),
                "marker {marker} should not become the description"
            );
        }
        // A blank line inside the block is a paragraph break, not its end.
        assert_eq!(
            front_value("description: |\n\n  after the gap", "description").as_deref(),
            Some("after the gap")
        );
        // An indicator with nothing indented under it yields no description, so the caller
        // still falls back instead of showing a pipe.
        assert_eq!(front_value("description: |\nname: x", "description"), None);
        // A plain scalar that merely starts with a pipe is text, not a block marker.
        assert_eq!(
            front_value("description: |pipes| in prose", "description").as_deref(),
            Some("|pipes| in prose")
        );
    }

    /// The two malformed files that still hold instructions: neither has usable
    /// frontmatter, so both fall back to the stem and to a generated description, and both
    /// still run.
    #[test]
    fn a_file_with_no_frontmatter_and_a_truncated_one_both_still_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("bare.md"), "# Just a heading\n\nDo the thing.").unwrap();
        // Opened a fence and stopped — the file the editor was killed halfway through.
        std::fs::write(root.join("cut.md"), "---\nname: cut\ndescription: Half w").unwrap();
        let (found, skipped) = read_root(root);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["bare", "cut"]
        );
        assert_eq!(found[0].description, "Run the bare skill");
        assert!(found[0].instructions.contains("Do the thing."));
        // The truncated one keeps its text as the body rather than losing it to a header
        // that never closed.
        assert!(found[1].instructions.contains("name: cut"));
        assert_eq!(found[1].description, "Run the cut skill");
    }

    #[test]
    fn an_unreadable_file_costs_that_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Not UTF-8: unreadable on every platform and for every user, root included.
        std::fs::write(root.join("binary.md"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        write_skill(
            root,
            "keeper",
            Layout::Flat,
            "name: keeper\ndescription: fine",
            TEST_BODY,
        );
        let (found, skipped) = read_root(root);
        assert_eq!(names_in(root), vec!["keeper"]);
        assert_eq!(found.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::Unreadable);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_the_user_cannot_open_is_reported_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(root, "locked", Layout::Flat, "name: locked", TEST_BODY);
        let locked = root.join("locked.md");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root ignores the mode; the assertion below would then be about the
        // test environment rather than about the code.
        if std::fs::read_to_string(&locked).is_ok() {
            return;
        }
        let (found, skipped) = read_root(root);
        assert!(found.is_empty());
        assert_eq!(skipped[0].reason, SkipReason::Unreadable);
    }

    #[test]
    fn a_file_too_large_to_carry_is_skipped_rather_than_half_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let huge = "x".repeat(MAX_SKILL_BYTES as usize + 1);
        write_skill(root, "huge", Layout::Flat, "name: huge", &huge);
        write_skill(root, "small", Layout::Flat, "name: small", TEST_BODY);
        let (found, skipped) = read_root(root);
        assert_eq!(
            found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["small"]
        );
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::TooLarge);
        // Its body never reaches a prompt: half a skill is not a skill.
        assert!(!found.iter().any(|s| s.instructions.contains("xxxx")));
    }

    #[test]
    fn a_root_stops_at_the_entry_cap_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for i in 0..MAX_ENTRIES_PER_ROOT + 5 {
            write_skill(root, &format!("s{i:04}"), Layout::Flat, "", TEST_BODY);
        }
        let (found, skipped) = read_root(root);
        assert_eq!(found.len(), MAX_ENTRIES_PER_ROOT);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::TooManyEntries);
        assert_eq!(skipped[0].path, root);
    }

    #[test]
    fn a_name_longer_than_the_help_column_can_take_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let long = "n".repeat(NAME_WIDTH + 1);
        write_skill(
            root,
            "long",
            Layout::Flat,
            &format!("name: {long}"),
            TEST_BODY,
        );
        let (found, skipped) = read_root(root);
        assert!(found.is_empty());
        assert_eq!(skipped[0].reason, SkipReason::UnusableName);
        // The boundary itself is fine.
        assert_eq!(
            command_name(&"n".repeat(NAME_WIDTH)).unwrap().len(),
            NAME_WIDTH
        );
    }

    // ─── what reaches the screen ─────────────────────────────────────────────────

    #[test]
    fn a_paragraph_description_is_clipped_to_one_row() {
        let long = "x".repeat(200);
        let short = render_safe(&long, DESCRIPTION_WIDTH);
        assert_eq!(short.chars().count(), DESCRIPTION_WIDTH);
        assert!(short.ends_with('…'));
        // Multi-line descriptions (common in the wild) keep only their first line.
        assert_eq!(
            render_safe("first line\nsecond line", DESCRIPTION_WIDTH),
            "first line"
        );
        // Multi-byte text is clipped on a char boundary, not a byte one.
        assert_eq!(
            render_safe(&"日".repeat(200), DESCRIPTION_WIDTH)
                .chars()
                .count(),
            DESCRIPTION_WIDTH
        );
    }

    /// The E5 gate: a description is outside input that a terminal surface prints
    /// verbatim, so nothing in it may still be an escape sequence by the time it is a row.
    #[test]
    fn an_escape_sequence_in_a_description_never_reaches_the_rendered_row() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(
            root,
            "sneaky",
            Layout::Flat,
            "name: sneaky\ndescription: \u{1b}[31mred\u{1b}[0m and \u{1b}]0;retitled\u{7}done",
            TEST_BODY,
        );
        let found = read_root(root).0;
        let rendered = &found[0].description;
        // Nothing that can move a cursor, repaint, or set a title survives — and no `[31m`
        // residue is left behind by removing the ESC alone.
        assert!(!rendered.chars().any(|c| c.is_control()), "{rendered:?}");
        assert!(!rendered.contains("31m"), "{rendered:?}");
        assert!(!rendered.contains("retitled"), "{rendered:?}");
        // The readable text is kept, not thrown away with the escapes.
        assert_eq!(rendered, "red and done");

        // The same gate, at the unit: C0 and C1 controls, and the bidi overrides that
        // reorder a row against its own bytes.
        // A stray control becomes the space it stood in for, rather than gluing the words
        // on either side of it into one.
        assert_eq!(render_safe("a\u{7}b\u{0}c", 72), "a b c");
        assert_eq!(render_safe("a\u{9c}b", 72), "a b");
        assert_eq!(render_safe("x\u{202e}drowssap\u{202c}y", 72), "xdrowssapy");
        // An escape at the very end, with nothing after it to consume, does not panic.
        assert_eq!(render_safe("tail\u{1b}", 72), "tail");
        assert_eq!(render_safe("tail\u{1b}[", 72), "tail");
        // Whitespace runs left by the substitutions collapse, so the column stays a column.
        assert_eq!(render_safe("a\u{1}\u{2}\u{3}b", 72), "a b");
    }

    #[test]
    fn a_description_that_sanitizes_to_nothing_falls_back_to_the_generated_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_skill(
            root,
            "blank",
            Layout::Flat,
            "name: blank\ndescription: \u{1b}[2J",
            TEST_BODY,
        );
        assert_eq!(read_root(root).0[0].description, "Run the blank skill");
    }

    // ─── the skip report ─────────────────────────────────────────────────────────

    #[test]
    fn the_skip_note_names_what_it_can_and_counts_the_rest() {
        assert_eq!(skipped_note(&[]), None);
        let one = skipped_note(&[Skipped {
            path: PathBuf::from("/s/broken.md"),
            reason: SkipReason::Unreadable,
        }])
        .unwrap();
        assert_eq!(one, "Skipped 1 skill entry: /s/broken.md (unreadable)");

        let many: Vec<Skipped> = (0..NOTE_ENTRIES + 2)
            .map(|i| Skipped {
                path: PathBuf::from(format!("/s/{i}.md")),
                reason: SkipReason::TooLarge,
            })
            .collect();
        let note = skipped_note(&many).unwrap();
        assert!(
            note.starts_with("Skipped 5 skill entries: /s/0.md (too large)"),
            "{note}"
        );
        assert!(note.ends_with(", and 2 more"), "{note}");

        // A path is outside input too: a file name carrying an escape sequence is
        // sanitized and clipped exactly like a description.
        let hostile = skipped_note(&[Skipped {
            path: PathBuf::from(format!("/s/\u{1b}[31m{}.md", "p".repeat(200))),
            reason: SkipReason::Unreadable,
        }])
        .unwrap();
        assert!(!hostile.chars().any(|c| c.is_control()), "{hostile:?}");
        assert!(!hostile.contains("31m"), "{hostile:?}");
        assert!(hostile.contains('…'), "{hostile:?}");
    }

    #[test]
    fn the_process_skip_list_is_empty_until_discover_records_one() {
        // `discover` is called once per process, from the entry point that knows the cwd;
        // a unit test that never calls it must see nothing rather than the machine's own
        // `~/.claude/skills` leaking in.
        assert!(skipped().is_empty());
    }

    // ─── the composed turn ───────────────────────────────────────────────────────

    #[test]
    fn the_composed_turn_carries_the_instructions_the_files_and_the_request() {
        let turn = compose(
            "critique",
            Path::new("/s/critique"),
            TEST_BODY,
            "  the caching plan  ",
        );
        assert!(turn.starts_with("Follow the skill \"critique\" for this turn."));
        assert!(turn.contains("Its files are in /s/critique."));
        assert!(turn.contains(TEST_BODY));
        assert!(turn.ends_with("The user's request: the caching plan"));
        // The order is fixed, not incidental: instructions first, the user's text last, so
        // a long body cannot bury the request in the middle of the turn.
        let body_at = turn.find(TEST_BODY).expect("body");
        let arg_at = turn.find("the caching plan").expect("arg");
        assert!(body_at < arg_at, "{turn}");
    }

    /// The documented answer to "what does a bare `/critique` do?": it runs, on the
    /// instructions alone. Whitespace-only is the same as none — a stray space must not
    /// produce a request section with nothing in it.
    #[test]
    fn a_skill_invoked_with_no_argument_runs_on_its_instructions_alone() {
        for arg in ["", "   ", "\t\n "] {
            let bare = compose("critique", Path::new("/s/critique"), TEST_BODY, arg);
            assert!(bare.ends_with(TEST_BODY), "{bare}");
            assert!(!bare.contains("The user's request"), "{bare}");
        }
    }

    /// Composition never slices, so nothing in it can land mid-character. The failure mode
    /// this rules out is a byte-indexed clip or split inside the body or the argument —
    /// which in Rust is a panic, not a mangled string.
    #[test]
    fn composition_is_multibyte_safe_in_the_body_and_in_the_argument() {
        let body = "レビュー担当者に、具体的な反論を三つ求めてください。";
        let arg = "  キャッシュ計画について — 🔍 見てほしい  ";
        let turn = compose("批評", Path::new("/s/日本語/critique"), body, arg);
        assert!(turn.contains(body), "{turn}");
        assert!(
            turn.contains("Its files are in /s/日本語/critique."),
            "{turn}"
        );
        // Trimmed on char boundaries, appended whole, and still last.
        assert!(
            turn.ends_with("The user's request: キャッシュ計画について — 🔍 見てほしい"),
            "{turn}"
        );
        assert!(turn.find(body).unwrap() < turn.find("キャッシュ計画").unwrap());
        // Every byte of both halves survived: nothing was dropped to make a boundary.
        assert!(turn.len() >= body.len() + arg.trim().len());
    }

    /// The heads-up a surface prints before it spends the turn. The size and the file are
    /// the point: naming the command alone would not tell a reader that pressing Enter
    /// just sent kilobytes they never typed.
    #[test]
    fn provenance_names_the_skill_its_size_and_the_file_it_came_from() {
        let file = Path::new("/proj/.claude/skills/critique/SKILL.md");
        let prompt = compose("critique", file.parent().unwrap(), TEST_BODY, "the plan");
        let line = provenance("critique", file, &prompt);
        assert_eq!(
            line,
            format!(
                "[skill /critique — {} B from /proj/.claude/skills/critique/SKILL.md]",
                prompt.len()
            )
        );
        // Kibibytes once a real skill body is in it, so "a few KB" reads as a few KB.
        let big = provenance("critique", file, &"x".repeat(3 * 1024 + 512));
        assert!(big.contains("3.5 KB from"), "{big}");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1024), "1.0 KB");

        // A path is outside input on the way to a terminal, exactly as in `skipped_note`:
        // an escape sequence in a directory name must not reach the screen as one.
        let hostile = provenance(
            "critique",
            &PathBuf::from(format!("/s/\u{1b}[31m{}/SKILL.md", "p".repeat(200))),
            &prompt,
        );
        assert!(!hostile.chars().any(|c| c.is_control()), "{hostile:?}");
        assert!(!hostile.contains("31m"), "{hostile:?}");
        assert!(hostile.contains('…'), "{hostile:?}");
        // Shortened from the front: a long path keeps the end, which is the half that says
        // WHICH skill. Clipping the tail instead would name no file at all.
        assert!(hostile.ends_with("/SKILL.md]"), "{hostile}");
        assert_eq!(
            render_path(Path::new("/s/critique/SKILL.md"), 60),
            "/s/critique/SKILL.md"
        );
        // Ellipsis included, the result is exactly `width` characters wide.
        let short = render_path(Path::new("/aaaa/bb/SKILL.md"), 12);
        assert_eq!(short, "…bb/SKILL.md");
        assert_eq!(short.chars().count(), 12);
        // Shortening is on char boundaries, not byte ones.
        let jp = render_path(Path::new("/日本語のディレクトリ/critique/SKILL.md"), 20);
        assert_eq!(jp.chars().count(), 20);
        assert!(jp.ends_with("/SKILL.md"), "{jp}");
    }

    // ─── a SKILL.md, end to end ──────────────────────────────────────────────────

    #[test]
    fn a_skill_from_either_layout_and_either_scope_becomes_a_command() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = test_registry(discover_in(&two_scopes(tmp.path())).specs);

        for surface in [Surface::Repl, Surface::Tui] {
            let names = reg.names_for(surface);
            assert_eq!(
                &names[names.len() - 2..],
                ["critique", "postmortem"],
                "both scopes joined {surface:?}, after the built-ins"
            );
        }
        // Not on `attach`: its worker serves a fixed set of verbs, and a skill is not one.
        assert_eq!(reg.names_for(Surface::Attach), names_for(Surface::Attach));

        // It parses through the one entry point, and the turn it produces is composed from
        // the file's own body — the state a `fn` pointer could not have carried.
        match reg.parse("/critique the caching plan", Surface::Repl) {
            Parsed::Command(SlashCommand::Skill {
                name,
                provenance,
                prompt,
            }) => {
                assert_eq!(name, "critique");
                assert!(prompt.contains(TEST_BODY), "{prompt}");
                assert!(
                    prompt.ends_with("The user's request: the caching plan"),
                    "{prompt}"
                );
                // The command carries what the surface must show before sending it: the
                // skill, the size of the turn, and the file it was composed out of.
                assert!(
                    provenance.starts_with("[skill /critique — "),
                    "{provenance}"
                );
                assert!(provenance.contains("critique/SKILL.md]"), "{provenance}");
            }
            other => panic!("expected the skill's command, got {other:?}"),
        }
        // Case is not a different command, and the help row reads off the frontmatter.
        let spec = reg
            .lookup("CRITIQUE", Surface::Tui)
            .expect("case-insensitive");
        assert_eq!(spec.description(), "Argue against the current plan");
        assert_eq!(spec.arg_hint(), "[text]");
        assert_eq!(spec.exec, ExecKind::Dispatch);
    }

    #[test]
    fn a_skill_that_claims_a_builtin_name_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".claude/skills");
        write_skill(
            &root,
            "compact",
            Layout::Directory,
            "name: compact\ndescription: hijack",
            TEST_BODY,
        );
        write_skill(
            &root,
            "keeper",
            Layout::Flat,
            "name: keeper\ndescription: fine",
            TEST_BODY,
        );
        let reg = test_registry(discover_in(&[root]).specs);
        // The built-in still resolves to its own command…
        assert!(matches!(
            reg.parse("/compact", Surface::Repl),
            Parsed::Command(SlashCommand::Compact)
        ));
        // …and the skill that did not collide is unaffected by its neighbour's fate.
        assert!(matches!(
            reg.parse("/keeper", Surface::Repl),
            Parsed::Command(SlashCommand::Skill { .. })
        ));
    }

    #[test]
    fn the_project_scope_wins_a_name_it_shares_with_the_user_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project/.claude/skills");
        let user = tmp.path().join("user/.claude/skills");
        write_skill(
            &project,
            "shared",
            Layout::Flat,
            "name: shared",
            "PROJECT BODY",
        );
        write_skill(&user, "shared", Layout::Flat, "name: shared", "USER BODY");
        let reg = test_registry(discover_in(&[project, user]).specs);
        match reg.parse("/shared", Surface::Repl) {
            Parsed::Command(SlashCommand::Skill { prompt, .. }) => {
                assert!(prompt.contains("PROJECT BODY"), "{prompt}");
                assert!(!prompt.contains("USER BODY"), "{prompt}");
            }
            other => panic!("expected the project skill, got {other:?}"),
        }
        assert_eq!(
            reg.names_for(Surface::Repl)
                .iter()
                .filter(|n| **n == "shared")
                .count(),
            1
        );
    }

    #[test]
    fn roots_are_project_then_user_and_never_the_same_directory_twice() {
        let in_repo = roots(Path::new("/work/repo"));
        assert_eq!(in_repo[0], PathBuf::from("/work/repo/.claude/skills"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(in_repo.len(), 2);
            assert_eq!(in_repo[1], home.join(".claude/skills"));
            // Started from $HOME, the same directory must not be read twice: the second
            // copy would collide with the first and be dropped, silently.
            assert_eq!(roots(&home).len(), 1);
        }
    }

    // ─── the REPL completion surface ─────────────────────────────────────────────
    // The TUI dropdown and the TUI router assert the same thing over the same fixture,
    // in their own modules (`tui::completion`, `tui::slash`).

    #[test]
    fn a_discovered_skill_completes_in_the_repl_exactly_as_a_builtin_does() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = test_registry(discover_in(&two_scopes(tmp.path())).specs);
        let candidates = crate::cli::repl::completion::command_candidates_in(reg, "crit");
        assert_eq!(candidates, vec!["critique"]);
        assert!(
            crate::cli::repl::completion::command_candidates_in(reg, "").contains(&"postmortem")
        );
    }

    /// The registry every surface reads is the built-in table until a caller installs the
    /// discovered layer, so a unit test never depends on what happens to sit in the
    /// machine's own `~/.claude/skills`.
    #[test]
    fn the_process_registry_is_builtins_only_until_a_caller_installs_extensions() {
        assert_eq!(
            Registry::resolve([]).names_for(Surface::Repl),
            names_for(Surface::Repl)
        );
    }
}
