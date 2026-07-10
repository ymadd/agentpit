//! Pure scorers for the gold-bench suite (design §2.1) — the half of the judge with no I/O.
//!
//! Every function here is *machine* judgement with no LLM and no process: it borrows a task's
//! grading metadata plus the candidate's raw output string and returns a `0.0..=1.0` score (or a
//! complexity/LOC metric). The structured-output extraction ([`extract_last_fence`],
//! [`parse_findings`]) and the F1 matchers ([`match_against`], [`f1`]) back the Review-family
//! graders; [`score_long_context`] is exact match; [`refactor_metric_norm`] is the metric term of
//! the refactor grade (its behaviour-equivalence gate lives in [`super::judge`]).
//!
//! Pure and immutable: matchers borrow their inputs and return a fresh score; nothing is mutated.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::suite::{
    AdversarialItem, AdversarialKind, Defect, Needle, RefactorGrading, SecurityDefect,
};

// ===========================================================================================
// Fenced-block + structured-output extraction
// ===========================================================================================

/// Extract the body of the **last** ```<lang> … ``` fenced block in `text` (case-insensitive on
/// the info string's first token). Returns `None` when no matching block is present. An
/// unterminated final block is returned as-is so a candidate that ends exactly at its code is
/// still graded.
pub fn extract_last_fence(text: &str, lang: &str) -> Option<String> {
    let lang = lang.to_ascii_lowercase();
    let mut last: Option<String> = None;
    let mut current: Option<String> = None;
    let mut in_other = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if current.is_some() {
                last = current.take();
            } else if in_other {
                in_other = false;
            } else {
                let info = rest.trim().to_ascii_lowercase();
                let first = info.split([' ', '\t', ',']).next().unwrap_or("");
                if first == lang.as_str() {
                    current = Some(String::new());
                } else {
                    in_other = true;
                }
            }
            continue;
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    last.or(current)
}

/// The trailing `[ … ]` span of `text`, a lenient fallback for an unfenced JSON array.
fn extract_bracketed_array(text: &str) -> Option<String> {
    let start = text.rfind('[')?;
    let end = text.rfind(']')?;
    (end > start).then(|| text[start..=end].to_string())
}

/// One reported finding from a Review-family candidate's trailing ```json array.
#[derive(Debug, Deserialize)]
struct ReportedFinding {
    #[serde(default)]
    line: u32,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    cwe: Option<String>,
}

/// Force-parse the candidate's structured output: the last ```json array (or a trailing bare
/// array), via `serde_json`. `None` signals a parse failure ⇒ score 0; `Some(vec![])` is the
/// valid "no defects" answer and is distinct from a failure.
fn parse_findings(output: &str) -> Option<Vec<ReportedFinding>> {
    let raw = extract_last_fence(output, "json").or_else(|| extract_bracketed_array(output))?;
    serde_json::from_str::<Vec<ReportedFinding>>(raw.trim()).ok()
}

// ===========================================================================================
// Review / Security / Adversarial matchers (F1)
// ===========================================================================================

/// Aggregated match counts for an F1 computation. `fp_weight` is fractional so adversarial
/// decoys can weigh 2.
struct MatchStats {
    tp: u32,
    fp_weight: f64,
    fn_count: u32,
}

/// True when two line numbers are within ±2 of each other (design §2.1).
fn line_close(a: u32, b: u32) -> bool {
    (i64::from(a) - i64::from(b)).abs() <= 2
}

/// Normalise a kind token for comparison: trimmed, lowercased.
fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// The numeric core of a CWE id (`"CWE-89"` → `"89"`), so `cwe-89` / `CWE 89` / `89` all agree.
fn cwe_digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// CWE-id equality: both sides must carry the same non-empty numeric id (design §2.3-2).
fn cwe_eq(reported: Option<&str>, expected: &str) -> bool {
    let want = cwe_digits(expected);
    !want.is_empty() && reported.map(cwe_digits).is_some_and(|got| got == want)
}

/// The dedup key for a false-positive report: its line plus its kind (or CWE when kind is blank).
fn fp_key(r: &ReportedFinding) -> String {
    let kind = norm(&r.kind);
    if kind.is_empty() {
        r.cwe.as_deref().map(cwe_digits).unwrap_or_default()
    } else {
        kind
    }
}

/// Greedily match reports to expected defects. Each expected defect matches at most one report;
/// repeated reports of an already-matched defect are dropped (dedup, design §2.1); reports that
/// match nothing are deduped false positives, weighted by `fp_weight`.
fn match_against<E>(
    expected: &[E],
    reported: &[ReportedFinding],
    is_match: impl Fn(&ReportedFinding, &E) -> bool,
    fp_weight: impl Fn(&ReportedFinding) -> f64,
) -> MatchStats {
    let mut taken = vec![false; expected.len()];
    let mut tp = 0u32;
    let mut fp = 0.0f64;
    let mut seen_fp: BTreeSet<(u32, String)> = BTreeSet::new();

    for r in reported {
        let hit = expected
            .iter()
            .enumerate()
            .find(|(i, e)| !taken[*i] && is_match(r, e))
            .map(|(i, _)| i);
        if let Some(i) = hit {
            taken[i] = true;
            tp += 1;
        } else if expected.iter().any(|e| is_match(r, e)) {
            // Duplicate of an already-matched real defect — neither TP nor FP.
        } else if seen_fp.insert((r.line, fp_key(r))) {
            fp += fp_weight(r);
        }
    }

    MatchStats {
        tp,
        fp_weight: fp,
        fn_count: expected.len() as u32 - tp,
    }
}

/// F1 from match stats. The zero-defect (noise / FP-resistance) case is undefined for F1, so it
/// uses the over-detection penalty `1/(1+FP)` instead (design §2.2).
fn f1(stats: &MatchStats, expected_len: usize) -> f64 {
    if expected_len == 0 {
        return 1.0 / (1.0 + stats.fp_weight);
    }
    let denom = 2.0 * f64::from(stats.tp) + stats.fp_weight + f64::from(stats.fn_count);
    if denom == 0.0 {
        0.0
    } else {
        2.0 * f64::from(stats.tp) / denom
    }
}

/// Score a plain review: ±2 line + kind match, F1; zero-defect tasks use `1/(1+FP)`.
pub fn score_review(defects: &[Defect], output: &str) -> f64 {
    let Some(reported) = parse_findings(output) else {
        return 0.0;
    };
    let stats = match_against(
        defects,
        &reported,
        |r, d| line_close(r.line, d.line) && norm(&r.kind) == norm(&d.kind),
        |_| 1.0,
    );
    f1(&stats, defects.len())
}

/// Score a security review: ±2 line + **CWE-id** match, F1; zero-defect tasks use `1/(1+FP)`.
pub fn score_security_review(defects: &[SecurityDefect], output: &str) -> f64 {
    let Some(reported) = parse_findings(output) else {
        return 0.0;
    };
    let stats = match_against(
        defects,
        &reported,
        |r, d| line_close(r.line, d.line) && cwe_eq(r.cwe.as_deref(), &d.cwe),
        |_| 1.0,
    );
    f1(&stats, defects.len())
}

/// Score an adversarial review: a real defect is a TP when a report locates it (±2 lines, closer
/// to it than to any decoy); reporting near a decoy is a hard FP at weight 2 (design §2.3-3) ⇒
/// weighted F1. Kind wording is advisory — locating the defect and resisting the decoy is the test.
pub fn score_adversarial(items: &[AdversarialItem], output: &str) -> f64 {
    let Some(reported) = parse_findings(output) else {
        return 0.0;
    };
    let reals: Vec<&AdversarialItem> = items
        .iter()
        .filter(|i| i.item == AdversarialKind::Real)
        .collect();
    let decoys: Vec<&AdversarialItem> = items
        .iter()
        .filter(|i| i.item == AdversarialKind::Decoy)
        .collect();
    let stats = match_against(
        &reals,
        &reported,
        // The load-bearing adversarial signal is *locating* the real defect while resisting the
        // decoy — not reproducing the gold's free-form `kind` slug verbatim (no model does; e.g.
        // codex answered "missing positive amount validation" for "missing-amount-validation").
        // A report credits a real defect when it lands within the ±2 line window AND sits closer
        // to that defect than to any decoy; a tie favours the decoy, so a report on a decoy
        // adjacent to the real (both gold ④ tasks place them 2 lines apart) never steals the
        // real's credit. Kind text is advisory here, unlike the plain Review grader.
        |r, e| {
            line_close(r.line, e.line)
                && !decoys.iter().any(|d| {
                    line_close(r.line, d.line) && d.line.abs_diff(r.line) <= e.line.abs_diff(r.line)
                })
        },
        |r| {
            if decoys.iter().any(|d| line_close(r.line, d.line)) {
                f64::from(AdversarialKind::DECOY_FP_WEIGHT)
            } else {
                1.0
            }
        },
    );
    f1(&stats, reals.len())
}

// ===========================================================================================
// LongContext
// ===========================================================================================

/// Score a long-context probe by exact match: `correct / N` (design §2.2).
pub fn score_long_context(needles: &[Needle], output: &str) -> f64 {
    if needles.is_empty() {
        return 0.0;
    }
    let correct = needles
        .iter()
        .filter(|n| exact_match(output, &n.expected))
        .count();
    correct as f64 / needles.len() as f64
}

/// Exact match: the whole trimmed output equals `expected`, or some trimmed line does.
fn exact_match(output: &str, expected: &str) -> bool {
    let want = expected.trim();
    output.trim() == want || output.lines().any(|l| l.trim() == want)
}

// ===========================================================================================
// Refactor metric normalisation (the pure half; the behaviour gate lives in `judge`)
// ===========================================================================================

/// Normalise complexity/LOC against their baselines: meeting or beating a baseline scores 1.0,
/// overshooting scales down. With no baselines, a passing gate is full marks.
pub(super) fn refactor_metric_norm(code: &str, grading: &RefactorGrading) -> f64 {
    let mut terms: Vec<f64> = Vec::new();
    if let Some(baseline) = grading.complexity_baseline {
        terms.push(ratio(baseline, cyclomatic_complexity(code)));
    }
    if let Some(baseline) = grading.loc_baseline {
        terms.push(ratio(baseline, effective_loc(code)));
    }
    if terms.is_empty() {
        1.0
    } else {
        terms.iter().sum::<f64>() / terms.len() as f64
    }
}

/// `min(1, baseline/actual)`: at-or-under the baseline is 1.0, worse decays toward 0.
fn ratio(baseline: u32, actual: u32) -> f64 {
    if actual == 0 {
        return 1.0;
    }
    (f64::from(baseline) / f64::from(actual)).min(1.0)
}

/// A cheap cyclomatic-complexity proxy: 1 plus the count of branch keywords/operators.
fn cyclomatic_complexity(code: &str) -> u32 {
    const BRANCH: [&str; 8] = ["if", "elif", "for", "while", "case", "match", "and", "or"];
    let lowered = code.to_ascii_lowercase();
    let keyword_branches: u32 = BRANCH.iter().map(|kw| count_word(&lowered, kw)).sum();
    let operator_branches = (code.matches("&&").count()
        + code.matches("||").count()
        + code.matches('?').count()) as u32;
    1 + keyword_branches + operator_branches
}

/// Non-blank, non-comment lines.
fn effective_loc(code: &str) -> u32 {
    code.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//"))
        .count() as u32
}

/// Count whole-word occurrences of `word` in `haystack` (alphanumeric/underscore boundaries).
fn count_word(haystack: &str, word: &str) -> u32 {
    let bytes = haystack.as_bytes();
    let mut count = 0u32;
    let mut from = 0usize;
    while let Some(pos) = haystack[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let right_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if left_ok && right_ok {
            count += 1;
        }
        from = start + 1;
    }
    count
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::bench::suite::{FixtureLang, HiddenTests};

    // ---- fence + structured-output extraction -------------------------------------------

    #[test]
    fn extract_last_fence_picks_the_last_matching_block() {
        let text = "first\n```python\nx = 1\n```\nmid\n```python\ny = 2\n```\n";
        assert_eq!(
            extract_last_fence(text, "python").as_deref(),
            Some("y = 2\n")
        );
    }

    #[test]
    fn extract_last_fence_filters_by_language() {
        let text = "```json\n[]\n```\n```rust\nfn main() {}\n```\n";
        assert_eq!(
            extract_last_fence(text, "rust").as_deref(),
            Some("fn main() {}\n")
        );
        assert!(extract_last_fence("no fences here", "python").is_none());
    }

    #[test]
    fn parse_findings_distinguishes_empty_from_failure() {
        let empty = "report:\n```json\n[]\n```";
        assert_eq!(parse_findings(empty).map(|v| v.len()), Some(0));

        let valid = "```json\n[{\"line\": 4, \"kind\": \"x\"}]\n```";
        assert_eq!(parse_findings(valid).map(|v| v.len()), Some(1));

        let broken = "```json\nnot json\n```";
        assert!(parse_findings(broken).is_none());

        assert!(parse_findings("prose with no array").is_none());
    }

    // ---- review / security / adversarial ------------------------------------------------

    fn defect(line: u32, kind: &str) -> Defect {
        Defect {
            line,
            kind: kind.to_string(),
            cwe: None,
        }
    }

    #[test]
    fn review_correct_finding_scores_full_f1() {
        let defects = [defect(4, "missing-null-check")];
        let out = "```json\n[{\"line\": 4, \"kind\": \"missing-null-check\"}]\n```";
        assert!((score_review(&defects, out) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn review_within_two_lines_still_matches_but_wrong_kind_misses() {
        let defects = [defect(4, "missing-null-check")];
        // ±2 line tolerance: reported at 6 still hits.
        let near = "```json\n[{\"line\": 6, \"kind\": \"missing-null-check\"}]\n```";
        assert!((score_review(&defects, near) - 1.0).abs() < 1e-9);

        // Wrong kind ⇒ FN + FP ⇒ score 0.
        let wrong = "```json\n[{\"line\": 4, \"kind\": \"unrelated\"}]\n```";
        assert_eq!(score_review(&defects, wrong), 0.0);
    }

    #[test]
    fn review_noise_task_rewards_empty_and_penalises_overdetection() {
        let none: [Defect; 0] = [];
        let clean = "```json\n[]\n```";
        assert!((score_review(&none, clean) - 1.0).abs() < 1e-9);

        let noisy = "```json\n[{\"line\": 1, \"kind\": \"made-up\"}]\n```";
        // 1/(1+1) = 0.5 — strictly below a clean report.
        assert!((score_review(&none, noisy) - 0.5).abs() < 1e-9);
        assert!(score_review(&none, noisy) < score_review(&none, clean));
    }

    #[test]
    fn review_parse_failure_scores_zero() {
        let defects = [defect(4, "x")];
        assert_eq!(score_review(&defects, "no json at all"), 0.0);
    }

    fn sec(line: u32, cwe: &str) -> SecurityDefect {
        SecurityDefect {
            line,
            kind: "k".to_string(),
            cwe: cwe.to_string(),
            severity: None,
        }
    }

    #[test]
    fn security_matches_on_cwe_and_misses_on_mismatch() {
        let defects = [sec(2, "CWE-89")];
        let hit = "```json\n[{\"line\": 2, \"kind\": \"sqli\", \"cwe\": \"CWE-89\"}]\n```";
        assert!((score_security_review(&defects, hit) - 1.0).abs() < 1e-9);

        // Right line, wrong CWE ⇒ no match ⇒ FN + FP ⇒ 0.
        let wrong_cwe = "```json\n[{\"line\": 2, \"kind\": \"sqli\", \"cwe\": \"CWE-22\"}]\n```";
        assert_eq!(score_security_review(&defects, wrong_cwe), 0.0);
    }

    #[test]
    fn security_fp_resistance_task_rewards_empty() {
        let none: [SecurityDefect; 0] = [];
        assert!((score_security_review(&none, "```json\n[]\n```") - 1.0).abs() < 1e-9);
        let noisy = "```json\n[{\"line\": 2, \"kind\": \"sqli\", \"cwe\": \"CWE-89\"}]\n```";
        assert!(score_security_review(&none, noisy) < 1.0);
    }

    fn adv(line: u32, kind: &str, item: AdversarialKind) -> AdversarialItem {
        AdversarialItem {
            line,
            kind: kind.to_string(),
            cwe: None,
            item,
        }
    }

    #[test]
    fn adversarial_real_only_beats_reporting_a_decoy() {
        let items = [
            adv(3, "missing-amount-validation", AdversarialKind::Real),
            adv(1, "comment-claims-safe", AdversarialKind::Decoy),
        ];

        let real_only = "```json\n[{\"line\": 3, \"kind\": \"missing-amount-validation\"}]\n```";
        assert!((score_adversarial(&items, real_only) - 1.0).abs() < 1e-9);

        // Catching the real AND flagging the decoy: the decoy is a weight-2 FP, dragging F1 down.
        let with_decoy = "```json\n[{\"line\": 3, \"kind\": \"missing-amount-validation\"},\
                          {\"line\": 1, \"kind\": \"comment-claims-safe\"}]\n```";
        let decoy_score = score_adversarial(&items, with_decoy);
        assert!(
            decoy_score < 1.0,
            "decoy report should reduce score, got {decoy_score}"
        );
        // 2*1 / (2*1 + 2 + 0) = 0.5.
        assert!((decoy_score - 0.5).abs() < 1e-9, "got {decoy_score}");
    }

    #[test]
    fn adversarial_credits_a_correct_line_despite_a_differently_worded_kind() {
        // The fix this dogfooding pass surfaced: a model that finds the real defect on the right
        // line and resists the decoy must score full marks even when its free-form `kind` differs
        // from the gold slug — codex answered "missing positive amount validation" (bare array, no
        // fence) for "missing-amount-validation" and was wrongly scored 0 under exact-kind match.
        let items = [
            adv(3, "missing-amount-validation", AdversarialKind::Real),
            adv(1, "comment-claims-safe", AdversarialKind::Decoy),
        ];
        let differing_kind = "[{\"line\": 3, \"kind\": \"missing positive amount validation\"}]";
        assert!((score_adversarial(&items, differing_kind) - 1.0).abs() < 1e-9);

        // A report that falls for the decoy (line 1, within the ±2 window of the real at line 3)
        // is still penalised, not credited: proximity to the decoy wins over the line tolerance.
        let only_decoy = "[{\"line\": 1, \"kind\": \"looks validated\"}]";
        assert!(score_adversarial(&items, only_decoy) < 1e-9);
    }

    #[test]
    fn adversarial_decoy_is_a_harder_fp_than_random_noise() {
        let items = [
            adv(3, "real", AdversarialKind::Real),
            adv(1, "decoy", AdversarialKind::Decoy),
        ];
        let near_decoy =
            "```json\n[{\"line\": 3, \"kind\": \"real\"},{\"line\": 1, \"kind\": \"x\"}]\n```";
        let far_noise =
            "```json\n[{\"line\": 3, \"kind\": \"real\"},{\"line\": 40, \"kind\": \"x\"}]\n```";
        assert!(
            score_adversarial(&items, near_decoy) < score_adversarial(&items, far_noise),
            "weight-2 decoy FP must hurt more than a weight-1 noise FP"
        );
    }

    // ---- long context -------------------------------------------------------------------

    #[test]
    fn long_context_exact_match() {
        let needles = [Needle {
            needle: "deploy.region".to_string(),
            expected: "ap-northeast-1".to_string(),
        }];
        assert_eq!(score_long_context(&needles, "ap-northeast-1"), 1.0);
        assert_eq!(
            score_long_context(&needles, "The region is ap-northeast-1, I think."),
            0.0
        );
        // A standalone answer line still counts.
        assert_eq!(
            score_long_context(&needles, "Answer:\nap-northeast-1\n"),
            1.0
        );
    }

    #[test]
    fn long_context_partial_credit() {
        let needles = [
            Needle {
                needle: "a".to_string(),
                expected: "45".to_string(),
            },
            Needle {
                needle: "b".to_string(),
                expected: "THX-1138".to_string(),
            },
        ];
        // Only the first needle is answered on its own line.
        assert!((score_long_context(&needles, "45\nwrong") - 0.5).abs() < 1e-9);
    }

    // ---- refactor metrics ---------------------------------------------------------------

    #[test]
    fn refactor_metric_norm_rewards_lean_code() {
        let grading = RefactorGrading {
            behavior_test: HiddenTests {
                lang: FixtureLang::Python,
                source: String::new(),
            },
            complexity_baseline: Some(4),
            loc_baseline: Some(12),
        };
        let lean = "def total_price(amount, tier):\n    rates = {'gold': 0.8}\n    return amount * rates.get(tier, 1.0)\n";
        assert!((refactor_metric_norm(lean, &grading) - 1.0).abs() < 1e-9);

        let bloated = (0..40)
            .map(|i| format!("if x == {i} and y or z:\n    pass"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(refactor_metric_norm(&bloated, &grading) < 1.0);
    }

    #[test]
    fn complexity_and_loc_are_pure_and_sane() {
        assert_eq!(cyclomatic_complexity("def f():\n    return 1\n"), 1);
        // one `if` + one `and` = 1 + 2 = 3.
        assert_eq!(cyclomatic_complexity("if a and b:\n    pass\n"), 3);
        // `elif` must not also count as `if`.
        assert_eq!(
            cyclomatic_complexity("if a:\n    pass\nelif b:\n    pass\n"),
            3
        );
        assert_eq!(effective_loc("# comment\n\ndef f():\n    return 1\n"), 2);
    }
}
