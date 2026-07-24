//! Network-isolated code-execution jail for the gold-bench suite (design §2.1).
//!
//! The candidate's extracted code is scaffolded into a temp dir and run under a macOS
//! `sandbox-exec` seatbelt profile that denies the network and any write outside the work + temp
//! dirs, under a 30s wall-clock ceiling. The run is reduced to a `passed/total` count by parsing
//! the test summary. When the jail binary is unavailable the grade is [`SandboxOutcome::Skipped`]
//! (logged, never silently passed); a timeout / crash / unparseable summary scores 0.
//!
//! All I/O is isolated behind a temp dir removed on drop ([`DirGuard`]).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::score::extract_last_fence;
use super::suite::{FixtureLang, HiddenTests};

/// Wall-clock ceiling for one sandboxed fixture run (design §2.1).
const SANDBOX_TIMEOUT: Duration = Duration::from_secs(30);

/// The raw outcome of running a sandboxed fixture: skipped, or `passed` of `total` checks.
/// A timeout / non-zero crash / unparseable run yields `passed == 0`, i.e. a score of 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SandboxOutcome {
    /// `sandbox-exec` was unavailable; not counted as pass or fail.
    Skipped,
    /// Ran and produced counts.
    Ran { passed: u32, total: u32 },
}

/// Run the candidate's extracted code against hidden tests in a network-isolated `sandbox-exec`
/// jail. Returns [`SandboxOutcome::Skipped`] (logged) when the jail binary is missing, and a
/// zero outcome on missing code / timeout / unparseable result.
pub fn run_hidden_tests(tests: &HiddenTests, output: &str) -> SandboxOutcome {
    if !sandbox_exec_available() {
        log_skip("sandboxed grade");
        return SandboxOutcome::Skipped;
    }
    // A missing host toolchain is *our* environment's problem, not the backend's answer —
    // grading anyway would misattribute "python3/cargo not installed" as the backend
    // scoring 0 and poison the profile (2026-07 eval, finding 2).
    if !lang_tool_available(tests.lang) {
        eprintln!(
            "agentpit: `{}` not found on PATH — skipping sandboxed grade (not counted as \
             pass or fail)",
            lang_tool(tests.lang)
        );
        return SandboxOutcome::Skipped;
    }
    let tag = lang_tag(tests.lang);
    let Some(code) = extract_last_fence(output, tag) else {
        eprintln!("agentpit: no ```{tag} code block in candidate output — scoring 0");
        return SandboxOutcome::Ran {
            passed: 0,
            total: 1,
        };
    };
    match run_in_sandbox(tests.lang, &code, &tests.source) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("agentpit: sandbox execution error: {e}");
            SandboxOutcome::Ran {
                passed: 0,
                total: 1,
            }
        }
    }
}

pub(super) fn lang_tag(lang: FixtureLang) -> &'static str {
    match lang {
        FixtureLang::Python => "python",
        FixtureLang::Rust => "rust",
    }
}

pub(super) fn log_skip(what: &str) {
    eprintln!(
        "agentpit: sandbox-exec not found — skipping {what} (not counted as pass; install macOS \
         sandbox-exec to enable code-execution grading)"
    );
}

/// Is the `sandbox-exec` jail binary available on this host?
pub(super) fn sandbox_exec_available() -> bool {
    if Path::new("/usr/bin/sandbox-exec").is_file() {
        return true;
    }
    on_path("sandbox-exec")
}

/// The host toolchain a fixture language needs inside the jail.
fn lang_tool(lang: FixtureLang) -> &'static str {
    match lang {
        FixtureLang::Python => "python3",
        FixtureLang::Rust => "cargo",
    }
}

/// Is the fixture language's toolchain on PATH? (The jail inherits PATH, so a PATH scan
/// mirrors what `sandbox-exec` will resolve.)
fn lang_tool_available(lang: FixtureLang) -> bool {
    on_path(lang_tool(lang))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Build the jail, scaffold the fixture, run it under a 30s timeout, and reduce to counts.
pub(super) fn run_in_sandbox(
    lang: FixtureLang,
    code: &str,
    tests: &str,
) -> io::Result<SandboxOutcome> {
    let work = make_workdir()?;
    let _guard = DirGuard(work.clone());
    let canon = fs::canonicalize(&work).unwrap_or_else(|_| work.clone());
    let policy = write_policy(&work, &canon)?;
    let cmd = match lang {
        FixtureLang::Python => {
            scaffold_python(&work, code, tests)?;
            python_command(&policy, &work)
        }
        FixtureLang::Rust => {
            scaffold_rust(&work, code, tests)?;
            rust_command(&policy, &work)
        }
    };
    let result = run_with_timeout(cmd, SANDBOX_TIMEOUT)?;
    Ok(interpret(lang, result.as_ref()))
}

/// Reduce a finished (or timed-out) run to counts. A timeout or unparseable summary scores 0.
fn interpret(lang: FixtureLang, result: Option<&ProcOutput>) -> SandboxOutcome {
    let Some((_, stdout, stderr)) = result else {
        return SandboxOutcome::Ran {
            passed: 0,
            total: 1,
        };
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    let parsed = match lang {
        FixtureLang::Python => parse_pytest(&combined),
        FixtureLang::Rust => parse_cargo(&combined),
    };
    match parsed {
        Some((passed, total)) if total > 0 => SandboxOutcome::Ran {
            passed: passed.min(total),
            total,
        },
        _ => SandboxOutcome::Ran {
            passed: 0,
            total: 1,
        },
    }
}

/// A self-contained Python driver: prefer `pytest`, fall back to a tiny in-process runner that
/// imports `test_solution.py` and executes its `test_*` functions, so grading still works on a
/// host without pytest. Both forms emit an `N passed, M failed` summary the parser reads.
const PY_DRIVER: &str = r#"import sys
try:
    import pytest
    sys.exit(pytest.main(["-q", "-p", "no:cacheprovider", "test_solution.py"]))
except ImportError:
    pass
import importlib.util, traceback
spec = importlib.util.spec_from_file_location("test_solution", "test_solution.py")
mod = importlib.util.module_from_spec(spec)
try:
    spec.loader.exec_module(mod)
except Exception:
    traceback.print_exc()
    print("0 passed, 1 failed")
    sys.exit(1)
fns = [v for k, v in vars(mod).items() if k.startswith("test") and callable(v)]
passed = 0
failed = 0
for fn in fns:
    try:
        fn()
        passed += 1
    except Exception:
        failed += 1
        traceback.print_exc()
print("%d passed, %d failed" % (passed, failed))
"#;

fn python_command(policy: &Path, work: &Path) -> Command {
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-f")
        .arg(policy)
        .arg("python3")
        .arg("-c")
        .arg(PY_DRIVER)
        .current_dir(work);
    cmd
}

fn rust_command(policy: &Path, work: &Path) -> Command {
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-f")
        .arg(policy)
        .arg("cargo")
        .arg("test")
        .arg("--offline")
        .arg("-q")
        .current_dir(work)
        .env("CARGO_TARGET_DIR", work.join("target"))
        .env("CARGO_HOME", work.join(".cargo"));
    cmd
}

fn scaffold_python(work: &Path, code: &str, tests: &str) -> io::Result<()> {
    fs::write(work.join("solution.py"), code)?;
    fs::write(work.join("test_solution.py"), tests)
}

fn scaffold_rust(work: &Path, code: &str, tests: &str) -> io::Result<()> {
    fs::create_dir_all(work.join("src"))?;
    fs::create_dir_all(work.join("tests"))?;
    fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"solution\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    fs::write(work.join("src").join("lib.rs"), code)?;
    fs::write(work.join("tests").join("integration.rs"), tests)
}

/// Write a seatbelt profile that denies network and any write outside the work + temp dirs.
/// (Last-match-wins SBPL: `allow default`, then re-deny network/writes, then re-allow the jail.)
fn write_policy(work: &Path, canon_work: &Path) -> io::Result<PathBuf> {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let body = format!(
        "(version 1)\n\
         (allow default)\n\
         (deny network*)\n\
         (deny file-write*)\n\
         (allow file-write*\n  (subpath \"{work}\")\n  (subpath \"{tmp}\")\n  (regex #\"^/dev/\"))\n",
        work = sb_escape(canon_work),
        tmp = sb_escape(&tmp),
    );
    let path = work.join("policy.sb");
    fs::write(&path, body)?;
    Ok(path)
}

fn sb_escape(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// A finished process: its exit status plus captured stdout and stderr bytes.
type ProcOutput = (ExitStatus, Vec<u8>, Vec<u8>);

/// Spawn `cmd`, draining stdout/stderr on threads, and either return its `(status, out, err)` or
/// `None` if it overran `timeout` (in which case the whole process *group* is killed).
///
/// The child gets its own process group: killing only the direct `sandbox-exec` child would
/// leave grandchildren (`cargo` → `rustc`, `python3`) alive holding the inherited pipe write
/// ends, and the reader threads' `read_to_end` joins below would then block forever
/// (2026-07 eval, finding 3).
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Option<ProcOutput>> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            kill_process_group(&mut child);
            let _ = child.wait();
            // Safe to join now: every writer in the group is dead, so the pipes hit EOF.
            let _ = out_reader.join();
            let _ = err_reader.join();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(Some((status, stdout, stderr)))
}

/// Kill the child's whole process group (`kill -KILL -<pgid>`; the child was spawned with
/// `process_group(0)` so its pgid is its own pid), then the child itself as a fallback.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .status();
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// Parse a pytest (or fallback-driver) summary into `(passed, total)`.
fn parse_pytest(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().rev().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("passed") || l.contains("failed") || l.contains("error")
    })?;
    count_pairs(line)
}

/// Parse one or more cargo `test result:` lines into a summed `(passed, total)`.
fn parse_cargo(text: &str) -> Option<(u32, u32)> {
    let joined: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("test result:"))
        .collect();
    if joined.is_empty() {
        return None;
    }
    count_pairs(&joined.join(" "))
}

/// Sum `<n> passed` and `<n> failed|error[s]` token pairs into `(passed, passed + bad)`.
/// `None` when no such pair is present (an unparseable run ⇒ score 0).
fn count_pairs(line: &str) -> Option<(u32, u32)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut passed = 0u32;
    let mut bad = 0u32;
    let mut found = false;
    for pair in tokens.windows(2) {
        let Ok(n) = pair[0]
            .trim_matches(|c: char| !c.is_ascii_digit())
            .parse::<u32>()
        else {
            continue;
        };
        let label: String = pair[1]
            .chars()
            .filter(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        match label.as_str() {
            "passed" => {
                passed += n;
                found = true;
            }
            "failed" | "error" | "errors" => {
                bad += n;
                found = true;
            }
            _ => {}
        }
    }
    found.then_some((passed, passed + bad))
}

/// A unique temp work dir under the system temp dir.
fn make_workdir() -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "agentpit-bench-{}-{nanos}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Removes its directory on drop, so a sandbox run never leaks temp files.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- sandbox summary parsing --------------------------------------------------------

    #[test]
    fn parse_pytest_summaries() {
        assert_eq!(parse_pytest("==== 2 passed in 0.01s ===="), Some((2, 2)));
        assert_eq!(parse_pytest("1 failed, 1 passed in 0.02s"), Some((1, 2)));
        assert_eq!(parse_pytest("3 passed, 1 failed"), Some((3, 4)));
        assert_eq!(parse_pytest("1 error in 0.01s"), Some((0, 1)));
        assert!(parse_pytest("no test summary here").is_none());
    }

    #[test]
    fn parse_cargo_sums_result_lines() {
        let ok = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;";
        assert_eq!(parse_cargo(ok), Some((1, 1)));
        let multi = "test result: ok. 0 passed; 0 failed; 0 ignored;\n\
                     test result: FAILED. 1 passed; 1 failed; 0 ignored;";
        assert_eq!(parse_cargo(multi), Some((1, 2)));
        assert!(parse_cargo("error[E0425]: cannot find value").is_none());
    }

    /// Eval finding 3 (2026-07): killing only the direct child left grandchildren holding
    /// the pipe write ends, so the reader joins blocked forever. `sh -c 'sleep 30; :'`
    /// keeps `sh` as the child and `sleep` as a grandchild on the same inherited pipes —
    /// with the old code this test hangs; with the group kill it returns promptly.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_the_whole_process_group_and_returns_none() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30; :");
        let start = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(200)).unwrap();
        assert!(result.is_none(), "a timed-out run reports None");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "reader joins must not block on surviving grandchildren"
        );
    }

    // ---- live sandbox (skips cleanly when sandbox-exec / python3 are unavailable) --------

    fn live_python_available() -> bool {
        let binaries_available = sandbox_exec_available()
            && Command::new("python3")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        binaries_available
            && matches!(
                run_in_sandbox(
                    FixtureLang::Python,
                    "def smoke():\n    return 1\n",
                    "from solution import smoke\n\ndef test_smoke():\n    assert smoke() == 1\n",
                ),
                Ok(SandboxOutcome::Ran {
                    passed: 1,
                    total: 1
                })
            )
    }

    const ADD_TESTS: &str = "from solution import add\n\n\
        def test_add():\n    assert add(2, 3) == 5\n\n\
        def test_neg():\n    assert add(-1, 1) == 0\n";

    #[test]
    fn live_correct_python_passes_all() {
        if !live_python_available() {
            eprintln!("skipping live sandbox test: functional Python sandbox unavailable");
            return;
        }
        let tests = HiddenTests {
            lang: FixtureLang::Python,
            source: ADD_TESTS.to_string(),
        };
        let output = "Sure, here it is:\n```python\ndef add(a, b):\n    return a + b\n```\n";
        match run_hidden_tests(&tests, output) {
            SandboxOutcome::Ran { passed, total } => {
                assert!(
                    total > 0 && passed == total,
                    "expected full pass, got {passed}/{total}"
                );
            }
            SandboxOutcome::Skipped => panic!("sandbox-exec available but grade was skipped"),
        }
    }

    #[test]
    fn live_broken_python_scores_zero() {
        if !live_python_available() {
            return;
        }
        let tests = HiddenTests {
            lang: FixtureLang::Python,
            source: ADD_TESTS.to_string(),
        };
        let output = "```python\ndef add(a, b):\n    return a - b\n```";
        if let SandboxOutcome::Ran { passed, .. } = run_hidden_tests(&tests, output) {
            assert_eq!(passed, 0, "a wrong solution must score 0");
        }
    }

    #[test]
    fn live_missing_code_block_scores_zero() {
        if !sandbox_exec_available() {
            return;
        }
        let tests = HiddenTests {
            lang: FixtureLang::Python,
            source: ADD_TESTS.to_string(),
        };
        // No ```python fence at all.
        match run_hidden_tests(&tests, "I refuse to write code.") {
            SandboxOutcome::Ran { passed, .. } => assert_eq!(passed, 0),
            SandboxOutcome::Skipped => {}
        }
    }
}
