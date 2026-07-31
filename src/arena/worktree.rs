//! Per-contender working-copy isolation for an arena round.
//!
//! An arena asks several backends to BUILD the same thing, which is what separates it from a
//! chat-style comparison: every contender edits files. Run them in one directory and they fight
//! over the same tree, so what the human ends up judging is interleaving, not capability.
//!
//! Each contender therefore gets its own detached `git worktree` off the current `HEAD`. The
//! worktree is a real checkout sharing the repo's object store, so setup is cheap and the
//! contender sees the project exactly as it is. When the dispatch finishes, its work is captured
//! as a **patch** — tracked changes via `git diff`, plus each new untracked file — and the
//! worktree is removed. The patch is what the vote is cast on, so nothing needs to stay on disk
//! for judging.
//!
//! Git is a hard requirement here, and deliberately so: without it there is no cheap way to
//! isolate N agents or to reduce their work to something comparable. A non-repo cwd is an error
//! with the reason stated, never a silent fallback to running them all in one directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

/// A checkout handed to one contender, removed when the round is done with it.
pub struct Worktree {
    path: PathBuf,
    repo: PathBuf,
}

impl Worktree {
    /// The directory to dispatch into.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort: a leaked worktree costs disk, but failing a finished round over cleanup
        // would throw away the results the human is waiting to judge. `--force` because the
        // contender left the tree dirty by design.
        let _ = git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.display().to_string(),
            ],
        );
    }
}

/// The repository root containing `cwd`, or an error explaining that the arena needs one.
pub fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| {
        anyhow!(
            "the arena runs each contender in its own git worktree, so it needs a git repository \
             ({}). Run it from inside a repo, or `git init` first.",
            e
        )
    })?;
    Ok(PathBuf::from(out.trim()))
}

/// Create a detached worktree off `HEAD` under the system temp dir. `tag` disambiguates
/// concurrent contenders (the run id plus the backend).
pub fn create(repo: &Path, tag: &str) -> Result<Worktree> {
    let path = std::env::temp_dir().join(format!("agentpit-arena-{tag}"));
    if path.exists() {
        bail!(
            "arena worktree {} already exists; remove it and retry",
            path.display()
        );
    }
    git(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &path.display().to_string(),
            "HEAD",
        ],
    )
    .with_context(|| format!("failed to create arena worktree at {}", path.display()))?;
    Ok(Worktree {
        path,
        repo: repo.to_path_buf(),
    })
}

/// What one contender left behind, reduced to something a human can read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capture {
    /// The unified diff of every text change.
    pub patch: String,
    /// Binary paths that were left OUT of the patch — reported, never silently dropped.
    pub binary: Vec<String>,
}

/// Everything the contender changed, as one patch: tracked edits plus each added file.
///
/// Untracked files are included on purpose — a contender that writes a brand-new module has done
/// most of its work outside `git diff`, and judging it on the tracked half alone would score the
/// wrong thing.
///
/// Binary files are excluded, because they are noise in a blind comparison rather than work. A
/// contender that runs the test suite leaves `__pycache__/*.pyc` behind (observed on the first
/// real round, 2026-07-31); git renders those as "Binary files differ", which a judge cannot
/// evaluate while they still inflate that submission's apparent size. Their paths come back in
/// [`Capture::binary`] so the omission is visible instead of silent.
pub fn capture_patch(tree: &Worktree) -> Result<Capture> {
    // `git add -AN` records new files as intent-to-add so the single `git diff` below covers
    // them, instead of stitching two different formats together.
    git(&tree.path, &["add", "-AN"]).context("failed to stage untracked files for the diff")?;

    // numstat reports binary files as `-	-	<path>`; that is the only reliable way to name them
    // before the diff itself has already flattened them to "Binary files ... differ".
    let numstat = git(&tree.path, &["diff", "--numstat"])
        .context("failed to list the contender's changes")?;
    let binary: Vec<String> = numstat
        .lines()
        .filter_map(|l| l.strip_prefix("-\t-\t"))
        .map(str::to_string)
        .collect();

    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into()];
    if !binary.is_empty() {
        args.push("--".into());
        args.push(".".into());
        args.extend(binary.iter().map(|p| format!(":(exclude){p}")));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let patch = git(&tree.path, &refs).context("failed to capture the contender's diff")?;
    Ok(Capture { patch, binary })
}

/// How large a patch is, in lines added/removed — shown next to a contender so an empty or
/// runaway submission is visible before reading it.
pub fn patch_size(patch: &str) -> (usize, usize) {
    let added = patch
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let removed = patch
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    (added, removed)
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_size_counts_hunk_lines_and_ignores_file_headers() {
        let patch = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,2 +1,3 @@
 keep
-gone
+new
+also new
";
        assert_eq!(patch_size(patch), (2, 1));
        assert_eq!(patch_size(""), (0, 0));
    }

    #[test]
    fn repo_root_explains_itself_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let err = repo_root(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("git repository"), "got: {msg}");
        assert!(msg.contains("git init"), "got: {msg}");
    }

    #[test]
    fn creates_an_isolated_checkout_and_captures_only_that_contenders_work() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "a@b.c"],
            vec!["config", "user.name", "t"],
        ] {
            git(repo, &args).unwrap();
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(repo, &["add", "."]).unwrap();
        git(repo, &["commit", "-qm", "base"]).unwrap();

        let a = create(repo, "test-a").unwrap();
        let b = create(repo, "test-b").unwrap();
        // Each contender sees the committed base, and neither sees the other's edits.
        assert_eq!(
            std::fs::read_to_string(a.path().join("base.txt")).unwrap(),
            "base\n"
        );
        std::fs::write(a.path().join("base.txt"), "changed by a\n").unwrap();
        std::fs::write(a.path().join("added.rs"), "fn a() {}\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(b.path().join("base.txt")).unwrap(),
            "base\n"
        );

        let cap_a = capture_patch(&a).unwrap();
        assert!(cap_a.patch.contains("changed by a"), "{}", cap_a.patch);
        // A brand-new file is most of some contenders' work, so it has to be in the patch.
        assert!(cap_a.patch.contains("added.rs"), "{}", cap_a.patch);
        assert!(cap_a.binary.is_empty());
        assert!(
            capture_patch(&b).unwrap().patch.is_empty(),
            "b changed nothing"
        );

        // The repo itself is untouched by either contender.
        assert_eq!(
            std::fs::read_to_string(repo.join("base.txt")).unwrap(),
            "base\n"
        );

        // A contender that runs the test suite leaves build artifacts behind; those are not work
        // and must not reach the judge.
        std::fs::create_dir_all(a.path().join("__pycache__")).unwrap();
        std::fs::write(a.path().join("__pycache__/x.pyc"), [0u8, 159, 146, 150]).unwrap();
        let with_artifact = capture_patch(&a).unwrap();
        assert_eq!(with_artifact.binary, vec!["__pycache__/x.pyc".to_string()]);
        assert!(
            !with_artifact.patch.contains("__pycache__"),
            "binary artifacts must not inflate the patch: {}",
            with_artifact.patch
        );
        assert!(
            with_artifact.patch.contains("changed by a"),
            "real work survives"
        );

        let path_a = a.path().to_path_buf();
        let path_b = b.path().to_path_buf();
        drop(a);
        drop(b);
        assert!(!path_a.exists(), "the worktree is removed on drop");
        assert!(
            !path_b.exists(),
            "each concurrently-live worktree is removed by its own guard"
        );
    }
}
