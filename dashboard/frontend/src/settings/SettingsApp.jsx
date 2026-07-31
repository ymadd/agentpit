import { useEffect, useMemo, useState } from "react";

import StudioApp from "../studio/StudioApp.jsx";
import { metaFor } from "../studio/backends.js";
import { indexModelCatalogs } from "../studio/model-catalogs.js";
import {
  autoUpdateEnabled,
  checkForUpdates,
  getUpdateSnapshot,
  installAvailableUpdate,
  cliLinkInstall,
  cliLinkRemove,
  cliLinkStatus,
  refreshSkills,
  restartDesktopApp,
  setAutoUpdateEnabled,
  subscribeToUpdates,
} from "./app-update.js";
import {
  arenaLeaderboard,
  arenaReveal,
  arenaRound,
  arenaRounds,
  arenaRun,
  arenaTemplates,
  arenaVote,
  pendingMatchups,
} from "./arena.js";
import { buildConfigPayload, draftFromConfig, ENSEMBLE_KEYS, validateConfigDraft } from "./config.js";
import "./settings.css";

const NAV = [
  { id: "general", table: "[default]", label: "基本", caption: "既定の動作" },
  { id: "routing", table: "[routes]", label: "ルーティング", caption: "タスクの振り分け" },
  { id: "backends", table: "[backends.*]", label: "バックエンド", caption: "CLI・モデル・接続" },
  { id: "ensembles", table: "[ensemble]", label: "アンサンブル", caption: "並列実行と集約" },
  { id: "workflow", table: "[workflow]", label: "ワークフロー", caption: "ロールと設計図" },
  { id: "arena", table: "arena", label: "アリーナ", caption: "対戦・判定・順位" },
  { id: "updates", table: "desktop", label: "アプリと更新", caption: "同梱CLI・自動更新" },
];

const ROUTES = [
  { key: "rescue", label: "Rescue", copy: "行き詰まりから復旧するタスク" },
  { key: "review", label: "Review", copy: "コードレビューと品質確認" },
  { key: "explain", label: "Explain", copy: "コードや設計の説明" },
  { key: "refactor", label: "Refactor", copy: "構造改善とリファクタリング" },
];

const ENSEMBLE_META = {
  default: ["標準アンサンブル", "agentpit ensemble"],
  review: ["レビュー", "agentpit review"],
  security_review: ["セキュリティレビュー", "agentpit security-review"],
  adversarial_review: ["反証レビュー", "agentpit adversarial-review"],
  rescue: ["Rescue", "agentpit rescue"],
  refactor: ["Refactor", "agentpit refactor"],
};

function BackendMark({ id, size = "normal" }) {
  // No id = the unpinned/auto state: a neutral dot instead of a backend monogram.
  const meta = id ? metaFor(id) : { mono: "·", color: "#7d8595" };
  return (
    <span className={`set-backend-mark ${size}`} style={{ "--backend-color": meta.color }} aria-hidden="true">
      {meta.mono}
    </span>
  );
}

function BackendSelect({ value, onChange, backends, allowEmpty = false, emptyLabel = "自動" }) {
  return (
    <select className="set-select" value={value || ""} onChange={(event) => onChange(event.target.value)}>
      {allowEmpty ? <option value="">{emptyLabel}</option> : null}
      {backends.map((backend) => (
        <option value={backend} key={backend}>{metaFor(backend).label}</option>
      ))}
    </select>
  );
}

function Switch({ checked, onChange, label, description }) {
  return (
    <label className="set-switch-row">
      <span>
        <strong>{label}</strong>
        {description ? <small>{description}</small> : null}
      </span>
      <input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} />
      <span className="set-switch-ui" aria-hidden="true" />
    </label>
  );
}

function SectionIntro({ eyebrow, title, copy, command }) {
  return (
    <div className="set-intro">
      <div className="set-eyebrow">{eyebrow}</div>
      <div className="set-title-line">
        <div>
          <h1>{title}</h1>
          <p>{copy}</p>
        </div>
        {command ? <code>{command}</code> : null}
      </div>
    </div>
  );
}

function Card({ children, className = "" }) {
  return <section className={`set-card ${className}`}>{children}</section>;
}

function GeneralPanel({ draft, change }) {
  return (
    <div className="set-page">
      <SectionIntro eyebrow="RUNTIME DEFAULTS" title="実行の起点" copy="明示指定がないときのバックエンドと、自動振り分けの入口を決めます。" command="[default]" />
      <div className="set-summary-strip" aria-label="設定の適用順">
        <span className="on">CLI flag</span><i>→</i><span>[routes]</span><i>→</i><span>auto_route</span><i>→</i><span>[default]</span>
      </div>
      <div className="set-two-col">
        <Card>
          <div className="set-card-kicker">DEFAULT BACKEND</div>
          <h2>最後に頼るエージェント</h2>
          <p>ツール別ルートにも自動条件にも一致しなかったタスクで使います。</p>
          <label className="set-field">
            <span>既定のバックエンド</span>
            <BackendSelect value={draft.defaults.backend} backends={draft.known_backends} onChange={(backend) => change((next) => { next.defaults.backend = backend; })} />
          </label>
          <div className="set-selected-backend">
            <BackendMark id={draft.defaults.backend} size="large" />
            <span><b>{metaFor(draft.defaults.backend).label}</b><small>fallback · {draft.defaults.backend}</small></span>
          </div>
        </Card>
        <Card>
          <div className="set-card-kicker">ROUTING MODE</div>
          <h2>自動ルーティング</h2>
          <p>プロンプトの長さとキーワードを見て、適したバックエンドへ切り替えます。</p>
          <Switch checked={draft.defaults.auto_route} onChange={(enabled) => change((next) => { next.defaults.auto_route = enabled; })} label={draft.defaults.auto_route ? "自動振り分けを使用" : "固定ルートのみ使用"} description="明示した --backend は常に最優先です。" />
          <div className={`set-mode-readout ${draft.defaults.auto_route ? "enabled" : ""}`}>
            <span className="set-signal" />
            {draft.defaults.auto_route ? "長文・レビュー条件を評価します" : "[routes] と [default] だけを使います"}
          </div>
        </Card>
      </div>
      <Card className="set-file-card">
        <div><span className="set-card-kicker">SOURCE OF TRUTH</span><h2>CLIとデスクトップで同じ設定を使用</h2></div>
        <code>{draft.config_path}</code>
        <span className={`set-file-state ${draft.exists ? "exists" : ""}`}>{draft.exists ? "読み込み済み" : "保存時に作成"}</span>
      </Card>
    </div>
  );
}

function RoutingPanel({ draft, change }) {
  return (
    <div className="set-page">
      <SectionIntro eyebrow="TASK ROUTING" title="タスクの行き先" copy="コマンド別の定番ルートと、内容に応じて上書きする自動条件を一画面で調整します。" command="[routes] + [auto_route]" />
      <div className="set-route-grid">
        {ROUTES.map((route) => (
          <Card key={route.key} className="set-route-card">
            <div className="set-route-head"><span>{route.label}</span><code>{route.key}</code></div>
            <p>{route.copy}</p>
            <div className="set-inline-backend">
              <BackendMark id={draft.routes[route.key]} />
              <BackendSelect allowEmpty emptyLabel="自動（学習ルーティング）" value={draft.routes[route.key]} backends={draft.known_backends} onChange={(backend) => change((next) => { next.routes[route.key] = backend; })} />
            </div>
          </Card>
        ))}
      </div>
      <Card>
        <div className="set-card-heading"><div><span className="set-card-kicker">CONTENT SIGNALS</span><h2>自動判定の条件</h2></div><span className={`set-status-pill ${draft.defaults.auto_route ? "on" : ""}`}>{draft.defaults.auto_route ? "有効" : "停止中"}</span></div>
        <div className="set-condition-grid">
          <label className="set-field">
            <span>長文とみなす文字数</span>
            <input className="set-input mono" type="number" min="0" value={draft.auto_route.long_context_threshold} onChange={(event) => change((next) => { next.auto_route.long_context_threshold = Number(event.target.value); })} />
            <small>この文字数を超えると長文用バックエンドへ送ります。</small>
          </label>
          <label className="set-field">
            <span>長文用バックエンド</span>
            <BackendSelect value={draft.auto_route.long_context_backend} backends={draft.known_backends} onChange={(backend) => change((next) => { next.auto_route.long_context_backend = backend; })} />
          </label>
          <label className="set-field wide">
            <span>レビュー判定キーワード</span>
            <input className="set-input mono" value={draft.auto_route.review_keywords_text} onChange={(event) => change((next) => { next.auto_route.review_keywords_text = event.target.value; })} placeholder="review, audit, critique, security" />
            <small>カンマ区切り。重複と空欄は保存時に整理されます。</small>
          </label>
          <label className="set-field">
            <span>レビュー用バックエンド</span>
            <BackendSelect value={draft.auto_route.review_backend} backends={draft.known_backends} onChange={(backend) => change((next) => { next.auto_route.review_backend = backend; })} />
          </label>
        </div>
      </Card>
    </div>
  );
}

function BackendsPanel({ draft, change, modelCatalogs }) {
  const defaults = { antigravity: "exec", claude: "exec", codex: "exec", opencode: "acp" };
  return (
    <div className="set-page">
      <SectionIntro eyebrow="AGENT CLIs" title="バックエンド" copy="CLIごとの接続方式と既定モデルを設定します。ロールやコマンドの --model はここより優先されます。" command="[backends.*]" />
      <div className="set-backend-grid">
        {draft.backends.map((entry) => {
          const meta = metaFor(entry.id);
          const catalog = modelCatalogs[entry.id];
          const models = catalog?.models || [];
          const listId = models.length ? `set-models-${entry.id}` : undefined;
          return (
            <Card key={entry.id} className="set-backend-card">
              <div className="set-backend-title">
                <BackendMark id={entry.id} size="large" />
                <div><h2>{meta.label}</h2><code>[backends.{entry.id}]</code></div>
              </div>
              <label className="set-field">
                <span>Transport</span>
                <div className="set-segmented">
                  {["", "exec", "acp"].map((transport) => (
                    <button type="button" key={transport || "default"} className={entry.transport === transport ? "on" : ""} onClick={() => change((next) => { next.backends.find((item) => item.id === entry.id).transport = transport; })}>
                      {transport || `既定 · ${defaults[entry.id] || "exec"}`}
                    </button>
                  ))}
                </div>
              </label>
              <label className="set-field">
                <span>既定モデル</span>
                <input className="set-input mono" value={entry.model} list={listId} onChange={(event) => change((next) => { next.backends.find((item) => item.id === entry.id).model = event.target.value; })} placeholder="CLI default" />
                {listId ? (
                  <datalist id={listId}>
                    {models.map((model) => (
                      <option key={model.value} value={model.value} label={model.label !== model.value ? model.label : undefined} />
                    ))}
                  </datalist>
                ) : null}
                <small>空欄なら各CLIの既定モデルを使用します。{listId ? `候補は ${catalog.source} から取得しています。` : null}</small>
              </label>
            </Card>
          );
        })}
      </div>
    </div>
  );
}

function EnsembleEditor({ id, entry, backends, change }) {
  const [title, command] = ENSEMBLE_META[id];
  const toggle = (backend) => change((next) => {
    const members = next.ensemble[id].members;
    next.ensemble[id].members = members.includes(backend) ? members.filter((member) => member !== backend) : [...members, backend];
  });
  const move = (index, delta) => change((next) => {
    const members = next.ensemble[id].members;
    const target = index + delta;
    if (target < 0 || target >= members.length) return;
    [members[index], members[target]] = [members[target], members[index]];
  });
  return (
    <Card className="set-ensemble-card">
      <div className="set-ensemble-head"><div><h2>{title}</h2><code>{command}</code></div><span>{entry.members.length} agents</span></div>
      <div className="set-member-picker">
        {backends.map((backend) => <button type="button" key={backend} className={entry.members.includes(backend) ? "on" : ""} onClick={() => toggle(backend)}><BackendMark id={backend} />{metaFor(backend).label}</button>)}
      </div>
      <div className="set-order-list">
        {entry.members.length === 0 ? <div className="set-empty-line">個別メンバー未設定 · コマンドの通常ルートを使用</div> : entry.members.map((backend, index) => (
          <div key={backend}><span className="set-order-index">{String(index + 1).padStart(2, "0")}</span><BackendMark id={backend} /><b>{metaFor(backend).label}</b><span className="set-order-actions"><button type="button" disabled={index === 0} onClick={() => move(index, -1)}>↑</button><button type="button" disabled={index === entry.members.length - 1} onClick={() => move(index, 1)}>↓</button></span></div>
        ))}
      </div>
      <label className="set-field set-aggregator">
        <span>Aggregator</span>
        <BackendSelect allowEmpty emptyLabel="なし · 結果を連結" value={entry.aggregator} backends={backends} onChange={(backend) => change((next) => { next.ensemble[id].aggregator = backend; })} />
      </label>
    </Card>
  );
}

function EnsemblesPanel({ draft, change }) {
  return (
    <div className="set-page">
      <SectionIntro eyebrow="PARALLEL EXECUTION" title="アンサンブル" copy="各コマンドで並列に呼ぶメンバー、その優先表示順、最終集約を担当するエージェントを管理します。" command="[ensemble]" />
      <div className="set-ensemble-grid">
        {ENSEMBLE_KEYS.map((key) => <EnsembleEditor key={key} id={key} entry={draft.ensemble[key]} backends={draft.known_backends} change={change} />)}
      </div>
    </div>
  );
}

function SkillsCard() {
  const [state, setState] = useState({ status: "idle", message: "" });
  const run = async () => {
    setState({ status: "running", message: "更新しています…" });
    try {
      const result = await refreshSkills();
      setState({ status: "done", message: result.message });
    } catch (error) {
      setState({ status: "error", message: String(error) });
    }
  };
  return (
    <Card>
      <span className="set-card-kicker">CLAUDE CODE</span>
      <h2>コマンドとスキル</h2>
      <p>このバージョンに同梱された <code>/agentpit:*</code> コマンドとスキルを、検出した <code>.claude/</code> すべてに再インストールします。アプリ更新時にも自動で実行されます。</p>
      <div className="set-skills-actions">
        <button className="set-secondary" disabled={state.status === "running"} onClick={run}>
          {state.status === "running" ? "更新中…" : "今すぐ更新"}
        </button>
        {state.message ? <span className={state.status === "error" ? "set-skills-note error" : "set-skills-note"}>{state.message}</span> : null}
      </div>
    </Card>
  );
}

/// Puts the bundled CLI on PATH.
///
/// The app is the primary distribution and owns the sidecar, but a terminal cannot reach into the
/// bundle — so people install the standalone CLI as well and the two drift apart silently. This
/// card removes the second copy: PATH points at the bundle through a small `exec` shim, and the
/// app's own updates carry the terminal along with them.
function CliLinkCard() {
  const [link, setLink] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);

  const load = async () => {
    try {
      setLink(await cliLinkStatus());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };
  useEffect(() => {
    load();
  }, []);

  const act = async (fn) => {
    setBusy(true);
    setError(null);
    try {
      setLink(await fn());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const state = link?.state ?? "absent";
  const copy = {
    linked: "ターミナルの agentpit はこのアプリの同梱CLIを指しています。アプリを更新すれば、ターミナル側も一緒に上がります。",
    stale: "シムが古い場所を指しています。アプリを移動した場合は貼り直してください。",
    foreign: "PATH 上に別の agentpit があります。アプリとは独立して古いままになるため、置き換えを推奨します。",
    absent: "ターミナルから agentpit を使うには、同梱CLIへのシムを PATH に置きます。",
    unavailable: "この起動には同梱CLIがありません（開発ビルド）。",
  }[state];

  return (
    <Card>
      <span className="set-card-kicker">BUNDLED CLI</span>
      <h2>ターミナルからも同じCLIを使う</h2>
      <p>{copy}</p>
      {link ? (
        <div className="set-cli-contract">
          <code>{link.shim_path}</code>
          <i>→</i>
          <code>{link.sidecar_path || "—"}</code>
        </div>
      ) : null}
      {link?.resolved_on_path ? (
        <p className="set-skills-note">
          いまのシェル: <code>{link.resolved_on_path}</code>
          {link.resolved_version ? ` (${link.resolved_version})` : ""}
        </p>
      ) : null}
      <div className="set-skills-actions">
        {state === "linked" ? (
          <button className="set-secondary" disabled={busy} onClick={() => act(cliLinkRemove)}>
            解除
          </button>
        ) : state === "unavailable" ? null : (
          <button
            className="set-primary"
            disabled={busy}
            onClick={() => act(() => cliLinkInstall(state === "foreign"))}
          >
            {busy ? "設置中…" : state === "foreign" ? "置き換えて設置" : "PATH に設置"}
          </button>
        )}
        {error ? <span className="set-skills-note error">{error}</span> : null}
      </div>
    </Card>
  );
}


/// The arena, as a desktop surface: start a round, judge it blind, read the standings.
///
/// The three live in one pane because they are one loop — a round is worth starting only if it
/// gets judged, and the standings mean nothing until it has been.
function ArenaPanel() {
  const [templates, setTemplates] = useState([]);
  const [rounds, setRounds] = useState([]);
  const [board, setBoard] = useState(null);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const [templateId, setTemplateId] = useState("");
  const [target, setTarget] = useState("");
  const [task, setTask] = useState("");
  const [cwd, setCwd] = useState("");
  const [contenders, setContenders] = useState([]);

  const [judging, setJudging] = useState(null);

  const load = async () => {
    try {
      const [t, r, b] = await Promise.all([arenaTemplates(), arenaRounds(), arenaLeaderboard()]);
      setTemplates(t || []);
      setRounds(r || []);
      setBoard(b);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };
  useEffect(() => {
    load();
  }, []);

  const chosen = templates.find((t) => t.id === templateId);
  const toggle = (id) =>
    setContenders((prev) => (prev.includes(id) ? prev.filter((b) => b !== id) : [...prev, id]));

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      setRounds((await arenaRun({ task, template: templateId || null, target, contenders, cwd })) || []);
      setTask("");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="set-page">
      <SectionIntro
        eyebrow="HUMAN VERDICT"
        title="アリーナ"
        copy="同じお題を各バックエンドに別々のworktreeで解かせ、結果を伏せたまま2つずつ見比べて選びます。票はそのまま学習ラベルになります。"
        command="agentpit arena"
      />
      {error ? <div className="set-error-banner"><span>{error}</span></div> : null}

      <Card>
        <span className="set-card-kicker">NEW ROUND</span>
        <h2>お題を出す</h2>
        <p>
          組み込みプローブは能力マトリクスの各セルに1つずつ対応していて、票が落ちるセルを自分で決めます。
          自由記述にすると、カテゴリは本文から推測されます。
        </p>
        <label className="set-field set-arena-field">
          <span>プローブ</span>
          <select className="set-select" value={templateId} onChange={(e) => setTemplateId(e.target.value)}>
            <option value="">（自由記述）</option>
            {templates.map((t) => (
              <option key={t.id} value={t.id}>
                {t.id} — {t.category}
              </option>
            ))}
          </select>
        </label>
        {chosen ? (
          <>
            <p className="set-skills-note">{chosen.probes}</p>
            {chosen.target ? (
              <label className="set-field set-arena-field">
                <span>対象</span>
                <input className="set-input" value={target} onChange={(e) => setTarget(e.target.value)} placeholder={chosen.target} />
              </label>
            ) : null}
          </>
        ) : (
          <label className="set-field set-arena-field">
            <span>お題</span>
            <input className="set-input" value={task} onChange={(e) => setTask(e.target.value)} placeholder="何を作らせるか" />
          </label>
        )}
        <label className="set-field set-arena-field">
          <span>作業ディレクトリ</span>
          <input className="set-input" value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="空欄でカレント（gitリポジトリであること）" />
        </label>
        <div className="set-arena-picks">
          {["claude", "codex", "antigravity", "opencode"].map((b) => (
            <button
              key={b}
              className={contenders.includes(b) ? "set-arena-pick on" : "set-arena-pick"}
              onClick={() => toggle(b)}
            >
              {b}
            </button>
          ))}
        </div>
        <div className="set-skills-actions">
          <button className="set-primary" disabled={busy || contenders.length < 2} onClick={start}>
            {busy ? "実行中… 数分かかります" : "ラウンド開始"}
          </button>
          {busy ? <span className="set-skills-note">各バックエンドが自分のworktreeで作業しています。</span> : null}
        </div>
      </Card>

      <Card>
        <span className="set-card-kicker">ROUNDS</span>
        <h2>判定待ち</h2>
        {rounds.length === 0 ? (
          <p>まだラウンドがありません。</p>
        ) : (
          <ul className="set-arena-rounds">
            {rounds.map((r) => (
              <li key={r.round_id}>
                <div>
                  <b>{(r.task || "").split("\n").filter((l) => !l.startsWith("CATEGORY:"))[0]}</b>
                  <small>
                    {r.contenders.join(" · ")} — {r.votes}/{r.matchups} 判定済み
                  </small>
                </div>
                <button
                  className="set-secondary"
                  disabled={r.pending === 0}
                  onClick={() => setJudging(r.round_id)}
                >
                  {r.pending === 0 ? "判定済み" : "判定する"}
                </button>
              </li>
            ))}
          </ul>
        )}
      </Card>

      {judging ? (
        <JudgeDialog
          roundId={judging}
          onClose={() => {
            setJudging(null);
            load();
          }}
        />
      ) : null}

      <ArenaBoard board={board} />
    </div>
  );
}

/// One blind comparison at a time. The identities are fetched only after the last pair is voted,
/// never between comparisons — seeing that A was your usual favourite would steer every remaining
/// pick in the round.
function JudgeDialog({ roundId, onClose }) {
  const [round, setRound] = useState(null);
  const [reveal, setReveal] = useState(null);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    arenaRound(roundId).then(setRound).catch((e) => setError(String(e)));
  }, [roundId]);

  const pending = pendingMatchups(round);
  const pair = pending[0];
  const sub = (label) => round?.submissions?.find((s) => s.label === label);

  const cast = async (winner, loser, tie) => {
    setBusy(true);
    try {
      await arenaVote(roundId, winner, loser, tie);
      const next = await arenaRound(roundId);
      setRound(next);
      if (pendingMatchups(next).length === 0) setReveal(await arenaReveal(roundId));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="set-arena-judge">
      <div className="set-arena-judge-hd">
        <b>{reveal ? "内訳" : pair ? `${pair[0]} と ${pair[1]} を見比べる` : "判定"}</b>
        <button className="set-close" onClick={onClose}>×</button>
      </div>
      {error ? <div className="set-error-banner"><span>{error}</span></div> : null}
      {reveal ? (
        <div className="set-arena-reveal">
          {reveal.submissions.map((s) => (
            <p key={s.label}>
              <b>{s.label}</b> = {s.backend}
              {s.model ? ` (${s.model}${s.effort ? " / " + s.effort : ""})` : ""}
            </p>
          ))}
        </div>
      ) : pair ? (
        <>
          <div className="set-arena-pair">
            {pair.map((label) => (
              <div key={label} className="set-arena-side">
                <div className="set-arena-side-hd">
                  <b>{label}</b>
                  <small>+{sub(label)?.added ?? 0}/-{sub(label)?.removed ?? 0}</small>
                </div>
                <pre>{sub(label)?.patch || ""}</pre>
              </div>
            ))}
          </div>
          <div className="set-skills-actions">
            <button className="set-primary" disabled={busy} onClick={() => cast(pair[0], pair[1], false)}>
              {pair[0]} が良い
            </button>
            <button className="set-primary" disabled={busy} onClick={() => cast(pair[1], pair[0], false)}>
              {pair[1]} が良い
            </button>
            <button className="set-secondary" disabled={busy} onClick={() => cast(pair[0], pair[1], true)}>
              甲乙つけがたい
            </button>
          </div>
        </>
      ) : (
        <p>このラウンドは判定済みです。</p>
      )}
    </div>
  );
}

/// Standings. The interval and the provisional flag are shown, never trimmed away: these ratings
/// rest on dozens of votes, not millions, and a table that hid that would invite exactly the
/// false confidence the arena exists to remove.
function ArenaBoard({ board }) {
  if (!board) return null;
  const rows = board.standings || [];
  return (
    <Card>
      <span className="set-card-kicker">LEADERBOARD</span>
      <h2>順位</h2>
      {rows.length === 0 ? (
        <p>まだ票がありません。</p>
      ) : (
        <>
          <table className="set-arena-board">
            <thead>
              <tr><th>バックエンド</th><th>スコア</th><th>90%区間</th><th>戦績</th></tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.backend} className={r.provisional ? "provisional" : ""}>
                  <td>{r.backend}</td>
                  <td>{r.score}</td>
                  <td>{r.low}–{r.high}</td>
                  <td>{r.wins}-{r.losses}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="set-skills-note">
            {board.votes} 票（うち引き分け {board.ties}）。
            {rows.some((r) => r.provisional)
              ? ` 薄い行は暫定 — 比較 ${board.min_comparisons} 件未満か、順位が入れ替わりうる幅です。測定値ではありません。`
              : ""}
          </p>
        </>
      )}
    </Card>
  );
}

function useUpdateState() {
  const [state, setState] = useState(getUpdateSnapshot);
  useEffect(() => subscribeToUpdates(setState), []);
  return state;
}

function UpdatesPanel() {
  const update = useUpdateState();
  const [automatic, setAutomatic] = useState(autoUpdateEnabled);
  useEffect(() => {
    const sync = (event) => setAutomatic(!!event.detail);
    window.addEventListener("agentpit:auto-update-setting", sync);
    return () => window.removeEventListener("agentpit:auto-update-setting", sync);
  }, []);
  const busy = update.status === "checking" || update.status === "installing";
  return (
    <div className="set-page">
      <SectionIntro eyebrow="DESKTOP DELIVERY" title="アプリと更新" copy="デスクトップアプリを本体として更新し、同じリリースのCLIをアプリ内に同梱します。" command="desktop + sidecar CLI" />
      <Card className="set-update-hero">
        <div className={`set-update-orb ${update.status}`}><span /></div>
        <div className="set-update-copy">
          <span className="set-card-kicker">AGENTPIT DESKTOP</span>
          <h2>{update.status === "restart" ? "更新の準備ができました" : update.info?.available ? `v${update.info.latest_version} を利用できます` : "アプリを最新に保つ"}</h2>
          <p>{update.error || update.message || "起動時に同梱CLIからリリースを確認します。"}</p>
          <div className="set-version-pairs">
            <span><small>Desktop</small><b>{update.info?.current_version ? `v${update.info.current_version}` : "—"}</b></span>
            <span><small>Bundled CLI</small><b>{update.info?.bundled_cli_version ? `v${update.info.bundled_cli_version}` : "—"}</b></span>
            <span><small>Latest</small><b>{update.info?.latest_version ? `v${update.info.latest_version}` : "—"}</b></span>
          </div>
        </div>
        <div className="set-update-actions">
          {update.status === "restart" ? <button className="set-primary" onClick={restartDesktopApp}>再起動して適用</button> : update.info?.available ? <button className="set-primary" disabled={busy} onClick={installAvailableUpdate}>今すぐ更新</button> : <button className="set-secondary" disabled={busy} onClick={() => checkForUpdates()}>更新を確認</button>}
        </div>
      </Card>
      <div className="set-two-col">
        <Card>
          <span className="set-card-kicker">AUTOMATION</span>
          <h2>起動時の自動更新</h2>
          <p>新しいリリースがあれば、バックグラウンドで取得して次の再起動に備えます。</p>
          <Switch checked={automatic} onChange={(enabled) => { setAutomatic(enabled); setAutoUpdateEnabled(enabled); }} label={automatic ? "自動更新：オン" : "自動更新：オフ"} description="オフでも「更新を確認」から手動実行できます。" />
        </Card>
        <CliLinkCard />
      </div>
      <SkillsCard />
    </div>
  );
}

export default function SettingsApp() {
  const [section, setSection] = useState("general");
  const [visitedWorkflow, setVisitedWorkflow] = useState(false);
  const [draft, setDraft] = useState(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState(null);
  const [modelCatalogs, setModelCatalogs] = useState({});
  const active = useMemo(() => NAV.find((item) => item.id === section) || NAV[0], [section]);

  const load = async () => {
    setError(null);
    try {
      const call = window.__TAURI__?.core?.invoke;
      const data = call ? await call("config_get") : window.__AGENTPIT_MOCK_CONFIG__ || {};
      setDraft(draftFromConfig(data));
      setDirty(false);
    } catch (loadError) {
      setError(String(loadError));
    }
  };
  useEffect(() => { load(); }, []);
  useEffect(() => {
    // Model discovery shells out to each CLI, so it stays off the config load path: the form is
    // usable immediately and the suggestions fill in when the probes return.
    const call = window.__TAURI__?.core?.invoke;
    const catalogs = call ? call("get_model_catalogs", { refresh: false }) : Promise.resolve(window.__AGENTPIT_MOCK_MODEL_CATALOGS__ || []);
    let alive = true;
    catalogs.then((data) => { if (alive) setModelCatalogs(indexModelCatalogs(data)); }).catch(() => {});
    return () => { alive = false; };
  }, []);

  const change = (mutator) => {
    setDraft((current) => {
      const next = structuredClone(current);
      mutator(next);
      return next;
    });
    setDirty(true);
  };
  const save = async () => {
    const validation = validateConfigDraft(draft);
    if (validation) { setError(validation); return; }
    setSaving(true);
    setError(null);
    try {
      const call = window.__TAURI__?.core?.invoke;
      if (call) await call("config_save", { payload: buildConfigPayload(draft) });
      await load();
    } catch (saveError) {
      setError(String(saveError));
    } finally {
      setSaving(false);
    }
  };
  const navigate = (id) => {
    setSection(id);
    if (id === "workflow") setVisitedWorkflow(true);
  };
  const close = () => {
    if (dirty && !window.confirm("保存していない設定があります。閉じますか？")) return;
    window.__agentpitCloseSettings?.();
  };

  return (
    <div className={"set-root" + (section === "workflow" ? " workflow-mode" : "")}>
      <aside className="set-rail">
        <div className="set-brand"><span className="set-brand-orb" /><div><b>agentpit</b><small>CONTROL PLANE</small></div></div>
        <div className="set-rail-label">CONFIGURATION</div>
        <nav>
          {NAV.map((item) => (
            <button key={item.id} className={section === item.id ? "on" : ""} onClick={() => navigate(item.id)} aria-label={item.label} title={`${item.label} · ${item.table}`}>
              <span className="set-nav-track"><i /></span>
              <span><code>{item.table}</code><b>{item.label}</b><small>{item.caption}</small></span>
            </button>
          ))}
        </nav>
        <div className="set-rail-foot"><span>CONFIG FILE</span><code title={draft?.config_path}>{draft?.config_path || "loading…"}</code></div>
      </aside>
      <section className="set-workspace">
        <header className="set-header">
          <div><span className="set-header-table">{active.table}</span><b>{active.label}</b><small>{active.caption}</small></div>
          <div className="set-header-actions">
            {error ? <span className="set-header-error" title={error}>設定エラー</span> : dirty ? <span className="set-unsaved"><i />未保存</span> : <span className="set-saved"><i />同期済み</span>}
            {section !== "workflow" && section !== "updates" ? <button className="set-save" disabled={!dirty || saving || !draft} onClick={save}>{saving ? "保存中…" : "変更を保存"}</button> : null}
            <button className="set-close" onClick={close} aria-label="設定を閉じる">×</button>
          </div>
        </header>
        {error ? <div className="set-error-banner"><b>設定を保存できません</b><span>{error}</span><button onClick={load}>再読み込み</button></div> : null}
        <div className={`set-workflow-stage ${section === "workflow" ? "active" : ""}`}>{visitedWorkflow ? <StudioApp embedded /> : null}</div>
        {section !== "workflow" ? (
          <main className="set-scroll">
            {!draft ? <div className="set-loading"><span /><p>設定ファイルを読み込んでいます…</p></div> : section === "general" ? <GeneralPanel draft={draft} change={change} /> : section === "routing" ? <RoutingPanel draft={draft} change={change} /> : section === "backends" ? <BackendsPanel draft={draft} change={change} modelCatalogs={modelCatalogs} /> : section === "ensembles" ? <EnsemblesPanel draft={draft} change={change} /> : section === "arena" ? <ArenaPanel /> : <UpdatesPanel />}
          </main>
        ) : null}
      </section>
    </div>
  );
}
