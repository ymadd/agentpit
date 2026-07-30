import { useCallback, useEffect, useMemo, useState } from "react";
import {
  fetchStatus,
  scoreColor,
  SOURCE_BADGE,
  pct,
  evidenceCells,
  donutSegments,
  timelineBars,
  dayLabel,
  age,
} from "./status.js";
import { makeT, detectLang } from "../studio/i18n.js";
import "./styles.css";

// Refresh cadence. The report is a fold over the whole event log rather than a delta, so
// this polls lazily and only while the overlay is open — the launcher itself needs one read
// (for the coverage pill) and nothing more.
const POLL_MS = 15000;

export default function LearningApp() {
  const [status, setStatus] = useState(null);
  const [error, setError] = useState(null);
  const [open, setOpen] = useState(false);
  const [lang, setLang] = useState(detectLang);
  const t = useMemo(() => makeT(lang), [lang]);

  const load = useCallback(async () => {
    try {
      const next = await fetchStatus();
      setStatus(next);
      setError(null);
    } catch (e) {
      setError(String(e?.message || e));
    }
    setLang(detectLang());
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // Only poll while the view is open: a closed overlay does not need fresh numbers, and the
  // fold re-reads the entire log every call.
  useEffect(() => {
    if (!open) return undefined;
    const id = setInterval(load, POLL_MS);
    return () => clearInterval(id);
  }, [open, load]);

  const coverage = status?.coverage;
  const learnedShare = coverage ? pct(coverage.learned + coverage.benchmarked, coverage.total) : 0;

  return (
    <>
      <button
        className={`lr-launcher ${learnedShare > 0 ? "live" : ""}`}
        onClick={() => setOpen(true)}
      >
        <span className="lr-dot" />
        <span>{t("Learning")}</span>
        {coverage ? (
          <span className="lr-count">{t("{n}% measured", { n: learnedShare })}</span>
        ) : null}
      </button>
      {open && (
        <Overlay
          t={t}
          status={status}
          error={error}
          onRefresh={load}
          onClose={() => setOpen(false)}
        />
      )}
    </>
  );
}

function Overlay({ t, status, error, onRefresh, onClose }) {
  const [selected, setSelected] = useState(null);

  useEffect(() => {
    const onKey = (e) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="lr-overlay">
      <div className="lr-head">
        <span className="lr-title">{t("Learning")}</span>
        {status ? (
          <span className="lr-meta">
            {t("{runs} runs · {labels} labels", { runs: status.runs, labels: status.labels })}
          </span>
        ) : null}
        <button className="lr-btn" onClick={onRefresh}>
          {t("Refresh")}
        </button>
        <button className="lr-btn lr-close" onClick={onClose}>
          {t("Close ✕")}
        </button>
      </div>

      {error ? <div className="lr-error">{error}</div> : null}
      {!status ? (
        <div className="lr-empty">{error ? null : t("Reading the capability matrix…")}</div>
      ) : (
        <div className="lr-body">
          <section className="lr-top">
            <CoverageCard t={t} status={status} />
            <EvidenceCard t={t} status={status} />
            <ReplayCard t={t} status={status} />
            <SimilarityCard t={t} status={status} />
          </section>

          <Matrix t={t} status={status} selected={selected} onSelect={setSelected} />
          {selected ? <CellDetail t={t} status={status} selected={selected} /> : null}
          <Pending t={t} status={status} />
          <Timeline t={t} status={status} />
          <RoutingTable t={t} status={status} />

          <footer className="lr-foot">
            <div>{status.sources.profiles}</div>
            <div>{status.sources.events}</div>
          </footer>
        </div>
      )}
    </div>
  );
}

const DONUT_R = 34;
const DONUT_C = 2 * Math.PI * DONUT_R;
const PROVENANCE_COLOR = {
  benchmarked: "var(--ok)",
  learned: "var(--ac)",
  seeded: "var(--line-3)",
};

// How much of the matrix is measured rather than guessed. The seeded slice is deliberately
// the flattest colour: it is the absence of evidence, not a third kind of it.
function CoverageCard({ t, status }) {
  const coverage = status.coverage;
  const segments = donutSegments(coverage, DONUT_C);
  const measured = pct(coverage.learned + coverage.benchmarked, coverage.total);
  return (
    <div className="lr-card lr-card-donut">
      <div className="lr-card-hd">{t("Matrix coverage")}</div>
      <div className="lr-donut-wrap">
        <svg className="lr-donut" viewBox="0 0 80 80" role="img" aria-label={t("Matrix coverage")}>
          <circle cx="40" cy="40" r={DONUT_R} className="lr-donut-track" />
          {segments.map((s) => (
            <circle
              key={s.key}
              cx="40"
              cy="40"
              r={DONUT_R}
              className="lr-donut-seg"
              stroke={PROVENANCE_COLOR[s.key]}
              strokeDasharray={`${s.len} ${DONUT_C}`}
              strokeDashoffset={s.offset}
            />
          ))}
          <text x="40" y="38" className="lr-donut-num">
            {measured}%
          </text>
          <text x="40" y="50" className="lr-donut-sub">
            {t("measured")}
          </text>
        </svg>
        <ul className="lr-legend">
          {segments.map((s) => (
            <li key={s.key}>
              <span className="lr-swatch" style={{ background: PROVENANCE_COLOR[s.key] }} />
              <span className="lr-legend-k">{t(s.key)}</span>
              <span className="lr-legend-v">{s.value}</span>
            </li>
          ))}
          <li className="lr-legend-total">
            <span className="lr-legend-k">{t("cells")}</span>
            <span className="lr-legend-v">{coverage.total}</span>
          </li>
        </ul>
      </div>
    </div>
  );
}

const MIX_KEYS = ["outcome", "grade", "rerun", "exit"];
// Evidence strength, mirroring the fold's own weights (outcome 3 · grade 2 · rerun 1 · exit ½).
const MIX_COLOR = {
  outcome: "var(--ok)",
  grade: "var(--ac)",
  rerun: "#8a7fd0",
  exit: "var(--line-3)",
};

function EvidenceCard({ t, status }) {
  const mix = status.label_mix;
  const total = MIX_KEYS.reduce((sum, k) => sum + (mix[k] || 0), 0);
  return (
    <div className="lr-card">
      <div className="lr-card-hd">{t("Evidence quality")}</div>
      {total === 0 ? (
        <div className="lr-card-empty">{t("No labelled run yet.")}</div>
      ) : (
        <>
          <div className="lr-mixbar">
            {MIX_KEYS.map((k) =>
              mix[k] ? (
                <span
                  key={k}
                  className="lr-mixbar-seg"
                  style={{ flexGrow: mix[k], background: MIX_COLOR[k] }}
                  title={`${k}: ${mix[k]}`}
                />
              ) : null
            )}
          </div>
          <ul className="lr-mixlist">
            {MIX_KEYS.map((k) => (
              <li key={k}>
                <span className="lr-swatch" style={{ background: MIX_COLOR[k] }} />
                <span className="lr-legend-k">{t(k)}</span>
                <span className="lr-legend-v">{mix[k] || 0}</span>
              </li>
            ))}
          </ul>
          <div className="lr-card-note">
            {t("A human verdict outweighs six exit codes. {n} labels total.", { n: total })}
          </div>
        </>
      )}
    </div>
  );
}

function ReplayCard({ t, status }) {
  const replay = status.routing.replay;
  return (
    <div className="lr-card">
      <div className="lr-card-hd">{t("Learned policy replay")}</div>
      {!replay ? (
        <div className="lr-card-empty">{t("No labelled run yet.")}</div>
      ) : (
        <>
          <div className="lr-bignum">
            {pct(replay.correct, replay.evaluable)}
            <span className="lr-bignum-unit">%</span>
          </div>
          <div className="lr-card-sub">
            {t("{correct} of {evaluable} evaluable decisions would have gone well", {
              correct: replay.correct,
              evaluable: replay.evaluable,
            })}
          </div>
          <div className="lr-card-note">
            {t(
              "{skipped} of {decisions} decisions had no label for the policy's pick and were skipped.",
              { skipped: replay.decisions - replay.evaluable, decisions: replay.decisions }
            )}
          </div>
        </>
      )}
    </div>
  );
}

function SimilarityCard({ t, status }) {
  const s = status.similarity;
  const state = !s.built
    ? t("not in this build")
    : !s.enabled
      ? t("disabled in config")
      : s.samples === 0
        ? t("no samples yet")
        : t("active");
  return (
    <div className="lr-card">
      <div className="lr-card-hd">{t("Similarity (kNN)")}</div>
      <div className="lr-card-state">{state}</div>
      <ul className="lr-kvlist">
        <li>
          <span className="lr-legend-k">{t("samples")}</span>
          <span className="lr-legend-v">{s.samples}</span>
        </li>
        <li>
          <span className="lr-legend-k">{t("good / bad")}</span>
          <span className="lr-legend-v">
            {s.good} / {s.bad}
          </span>
        </li>
        <li>
          <span className="lr-legend-k">{t("needs")}</span>
          <span className="lr-legend-v">{s.min_samples}</span>
        </li>
      </ul>
      {!s.built ? (
        <div className="lr-card-note">
          {t("This build routes on the profile matrix alone (no --features similarity).")}
        </div>
      ) : null}
    </div>
  );
}

// The matrix. Colour = score, badge = provenance, a dot marks cells with live telemetry
// behind them. Seeded cells are dimmed: they are priors, not measurements.
function Matrix({ t, status, selected, onSelect }) {
  const categories = status.categories;
  const isSel = (backend, category) =>
    selected && selected.backend === backend && selected.category === category;

  return (
    <section className="lr-section">
      <h3 className="lr-h3">
        {t("Capability matrix")}
        <span className="lr-h3-note">
          {t("colour = score · badge = provenance · dot = has labels")}
        </span>
      </h3>
      <div className="lr-matrix-scroll">
        <table className="lr-matrix">
          <thead>
            <tr>
              <th className="lr-matrix-corner" />
              {categories.map((c) => (
                <th key={c} className="lr-matrix-col" title={t(c)}>
                  {/* Long category names are ellipsised to keep the grid readable, so the
                      full name has to survive somewhere — hence the title. */}
                  <span>{t(c)}</span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {status.rows.map((row) => (
              <tr key={row.backend}>
                <th className="lr-matrix-row">{row.backend}</th>
                {categories.map((category) => {
                  const cell = row.cells.find((c) => c.category === category);
                  if (!cell)
                    return (
                      <td key={category} className="lr-cell lr-cell-none">
                        —
                      </td>
                    );
                  return (
                    <td key={category} className="lr-cell-td">
                      <button
                        className={`lr-cell src-${cell.source} ${
                          isSel(row.backend, category) ? "sel" : ""
                        }`}
                        style={{ background: scoreColor(cell.value) }}
                        onClick={() =>
                          onSelect(
                            isSel(row.backend, category)
                              ? null
                              : { backend: row.backend, category }
                          )
                        }
                        title={`${row.backend} / ${category}: ${cell.value} (${cell.source}, conf ${cell.confidence.toFixed(2)}, ${cell.samples} samples)`}
                      >
                        <span className="lr-cell-v">{cell.value}</span>
                        <span className="lr-cell-src">{SOURCE_BADGE[cell.source] || "?"}</span>
                        {cell.evidence ? <span className="lr-cell-ev" /> : null}
                      </button>
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function CellDetail({ t, status, selected }) {
  const row = status.rows.find((r) => r.backend === selected.backend);
  const cell = row?.cells.find((c) => c.category === selected.category);
  if (!cell) return null;
  const e = cell.evidence;
  return (
    <section className="lr-detail">
      <div className="lr-detail-hd">
        {selected.backend} · {t(selected.category)}
      </div>
      <dl className="lr-kv">
        <dt>{t("Stored score")}</dt>
        <dd>
          {cell.value} <span className="lr-dim">({t(cell.source)})</span>
        </dd>
        <dt>{t("Confidence")}</dt>
        <dd>{cell.confidence.toFixed(2)}</dd>
        <dt>{t("Samples")}</dt>
        <dd>{cell.samples}</dd>
        {row.measured_at ? (
          <>
            <dt>{t("Measured at")}</dt>
            <dd>{row.measured_at}</dd>
          </>
        ) : null}
      </dl>
      {!e ? (
        <div className="lr-card-empty">{t("No labelled run has touched this cell.")}</div>
      ) : (
        <dl className="lr-kv">
          <dt>{t("Labels")}</dt>
          <dd>
            {e.labels} / {status.min_samples} {t("to promote")}
          </dd>
          <dt>{t("good / bad")}</dt>
          <dd>
            {e.good} / {e.bad}
          </dd>
          <dt>{t("Would score")}</dt>
          <dd>
            {e.projected}{" "}
            <span className="lr-dim">
              ({t("confidence")} {e.projected_confidence.toFixed(2)})
            </span>
          </dd>
          <dt>{t("Newest label")}</dt>
          <dd>{age(e.last_ts, status.generated_ts) || "—"}</dd>
        </dl>
      )}
    </section>
  );
}

// Cells with telemetry behind them, against the promotion gate. This is the part `profile
// show` cannot express: the matrix shows what landed, this shows what is still accruing.
function Pending({ t, status }) {
  const cells = evidenceCells(status);
  return (
    <section className="lr-section">
      <h3 className="lr-h3">
        {t("Evidence per cell")}
        <span className="lr-h3-note">
          {t("a cell needs {n} labels before the fold writes it", { n: status.min_samples })}
        </span>
      </h3>
      {cells.length === 0 ? (
        <div className="lr-empty-box">
          {t("No cell has labelled evidence yet. Dispatch work, then note the outcome.")}
        </div>
      ) : (
        <ul className="lr-pending">
          {cells.map(({ backend, cell, evidence }) => {
            const ratio = Math.min(1, evidence.labels / Math.max(1, status.min_samples));
            const state = evidence.outranked
              ? "outranked"
              : evidence.promoted
                ? "promoted"
                : "accruing";
            return (
              <li key={`${backend}-${cell.category}`} className={`lr-pending-row st-${state}`}>
                <span className="lr-pending-who">
                  {backend} <span className="lr-dim">/ {t(cell.category)}</span>
                </span>
                <span className="lr-progress" title={`${evidence.labels}/${status.min_samples}`}>
                  <span className="lr-progress-fill" style={{ width: `${ratio * 100}%` }} />
                  <span className="lr-progress-txt">
                    {evidence.labels}/{status.min_samples}
                  </span>
                </span>
                <span className="lr-pending-mix">
                  {MIX_KEYS.map((k) =>
                    evidence.mix[k] ? (
                      <span key={k} className="lr-chip" style={{ borderColor: MIX_COLOR[k] }}>
                        {t(k)} {evidence.mix[k]}
                      </span>
                    ) : null
                  )}
                </span>
                <span className="lr-pending-proj">
                  {cell.value} <span className="lr-arrow">→</span> {evidence.projected}
                </span>
                <span className="lr-pending-state">
                  {state === "outranked"
                    ? t("benchmarked — learned cannot overwrite")
                    : state === "promoted"
                      ? t("promoted")
                      : t("accruing")}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

function Timeline({ t, status }) {
  const { max, bars } = timelineBars(status.timeline);
  return (
    <section className="lr-section">
      <h3 className="lr-h3">
        {t("Labels per day")}
        <span className="lr-h3-note">
          {t("last {n} days · green good / red bad", { n: bars.length })}
        </span>
      </h3>
      {max === 0 ? (
        <div className="lr-empty-box">{t("No labels in this window.")}</div>
      ) : (
        <div className="lr-chart">
          {bars.map((b) => (
            <div
              key={b.start_ms}
              className="lr-chart-col"
              title={`${b.labels} (${b.good}/${b.bad})`}
            >
              <div className="lr-chart-stack">
                <div className="lr-chart-bad" style={{ height: `${b.badRatio * 100}%` }} />
                <div className="lr-chart-good" style={{ height: `${b.goodRatio * 100}%` }} />
              </div>
              <div className="lr-chart-x">{dayLabel(b.start_ms)}</div>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

// Where each category routes now, and whether measurement moved it off the prior. This is
// the only place the learning becomes a *decision* rather than a number.
function RoutingTable({ t, status }) {
  const routing = status.routing;
  return (
    <section className="lr-section">
      <h3 className="lr-h3">
        {t("Where each category routes")}
        <span className="lr-h3-note">
          {t("quality margin {n} · available: {list}", {
            n: routing.quality_margin,
            list: routing.available.join(", ") || "—",
          })}
        </span>
      </h3>
      {!routing.auto_route ? (
        <div className="lr-warn">
          {t(
            "auto_route is off — dispatch uses the default backend and never consults this matrix."
          )}
        </div>
      ) : null}
      {routing.pinned.length > 0 ? (
        <div className="lr-warn">
          {t("[routes] pins {list} — capability routing does not run for those tools.", {
            list: routing.pinned.join(", "),
          })}
        </div>
      ) : null}
      <ul className="lr-picks">
        {routing.picks.map((p) => (
          <li key={p.category} className={p.changed ? "changed" : ""}>
            <span className="lr-pick-cat">{t(p.category)}</span>
            <span className="lr-pick-arrow">→</span>
            <span className="lr-pick-be">{p.backend || t("unrouted")}</span>
            {p.score != null ? (
              <span className="lr-pick-score" style={{ background: scoreColor(p.score) }}>
                {p.score}
              </span>
            ) : null}
            {p.source ? <span className={`lr-pick-src src-${p.source}`}>{t(p.source)}</span> : null}
            {p.cost_tiebreak ? <span className="lr-tag">{t("cost tiebreak")}</span> : null}
            {p.changed ? (
              <span className="lr-pick-changed">
                {t("moved from {backend}", { backend: p.seeded_backend })}
              </span>
            ) : null}
          </li>
        ))}
      </ul>
    </section>
  );
}
