import { useEffect, useMemo, useState } from "react";

import StudioApp from "../studio/StudioApp.jsx";
import { metaFor } from "../studio/backends.js";
import {
  autoUpdateEnabled,
  checkForUpdates,
  getUpdateSnapshot,
  installAvailableUpdate,
  restartDesktopApp,
  setAutoUpdateEnabled,
  subscribeToUpdates,
} from "./app-update.js";
import { buildConfigPayload, draftFromConfig, ENSEMBLE_KEYS, validateConfigDraft } from "./config.js";
import "./settings.css";

const NAV = [
  { id: "general", table: "[default]", label: "基本", caption: "既定の動作" },
  { id: "routing", table: "[routes]", label: "ルーティング", caption: "タスクの振り分け" },
  { id: "backends", table: "[backends.*]", label: "バックエンド", caption: "CLI・モデル・接続" },
  { id: "ensembles", table: "[ensemble]", label: "アンサンブル", caption: "並列実行と集約" },
  { id: "workflow", table: "[workflow]", label: "ワークフロー", caption: "ロールと設計図" },
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
  const meta = metaFor(id);
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
              <BackendSelect value={draft.routes[route.key]} backends={draft.known_backends} onChange={(backend) => change((next) => { next.routes[route.key] = backend; })} />
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

function BackendsPanel({ draft, change }) {
  const defaults = { antigravity: "exec", gemini: "exec", claude: "exec", codex: "exec", opencode: "acp" };
  return (
    <div className="set-page">
      <SectionIntro eyebrow="AGENT CLIs" title="バックエンド" copy="CLIごとの接続方式と既定モデルを設定します。ロールやコマンドの --model はここより優先されます。" command="[backends.*]" />
      <div className="set-backend-grid">
        {draft.backends.map((entry) => {
          const meta = metaFor(entry.id);
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
                <input className="set-input mono" value={entry.model} onChange={(event) => change((next) => { next.backends.find((item) => item.id === entry.id).model = event.target.value; })} placeholder="CLI default" />
                <small>空欄なら各CLIの既定モデルを使用します。</small>
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
        <Card>
          <span className="set-card-kicker">BUNDLED CLI</span>
          <h2>アプリと同じバージョンを同梱</h2>
          <p>ワークフロー生成・説明・更新は、PATH上の別バージョンではなくアプリ内のCLIを優先します。</p>
          <div className="set-cli-contract"><code>agentpit</code><span>sidecar</span><i>→</i><code>config.toml</code></div>
        </Card>
      </div>
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
            {!draft ? <div className="set-loading"><span /><p>設定ファイルを読み込んでいます…</p></div> : section === "general" ? <GeneralPanel draft={draft} change={change} /> : section === "routing" ? <RoutingPanel draft={draft} change={change} /> : section === "backends" ? <BackendsPanel draft={draft} change={change} /> : section === "ensembles" ? <EnsemblesPanel draft={draft} change={change} /> : <UpdatesPanel />}
          </main>
        ) : null}
      </section>
    </div>
  );
}
