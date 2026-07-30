// Studio i18n. Mirrors the legacy dashboard: English source strings are the keys;
// LANG is shared via localStorage["agentpit.lang"] so a toggle here and the legacy
// app.js stay in sync. t(s) returns the JA translation when LANG==="ja", else the
// English key; `{var}` placeholders interpolate.

export const LANG_KEY = "agentpit.lang";

export function detectLang() {
  try {
    const s = localStorage.getItem(LANG_KEY);
    if (s === "ja" || s === "en") return s;
  } catch {
    // ignore
  }
  return (navigator.language || "en").toLowerCase().startsWith("ja") ? "ja" : "en";
}

// Persist + notify the legacy app.js (which owns the rest of the dashboard).
export function persistLang(lang) {
  const l = lang === "ja" ? "ja" : "en";
  try {
    localStorage.setItem(LANG_KEY, l);
  } catch {
    // ignore
  }
  if (typeof window !== "undefined" && window.__agentpitSetLang) window.__agentpitSetLang(l);
  return l;
}

// English source → Japanese. Keys must stay byte-identical to the rendered English.
export const JA = {
  // topbar
  BLUEPRINT: "設計図",
  "(default) workflow": "（既定）ワークフロー",
  "(default)": "（既定）",
  "＋ New workflow": "＋ 新規ワークフロー",
  "drag a handle → handle to draw an arrow": "ハンドルからハンドルへドラッグして矢印を引く",
  "Saving…": "保存中…",
  "Unsaved config": "未保存の設定",
  Saved: "保存済み",
  "✨ Generate": "✨ 生成",
  "Save config": "設定を保存",
  "Close ✕": "閉じる ✕",
  // banners
  "Some roles or workflow types have invalid names (empty, reserved, or duplicate). Fix the highlighted items before saving.":
    "ロールまたはワークフロータイプの名前が不正です（空・予約語・重複）。強調された項目を修正してから保存してください。",
  "The model returned an empty description.": "モデルが空の説明を返しました。",
  // palette
  PALETTE: "パレット",
  "drag a CLI/role onto a step · a saved step onto the canvas": "CLI／ロールはステップへ、保存ステップはキャンバスへドラッグ",
  CLIs: "CLI",
  "CAST · roles": "キャスト · ロール",
  "No roles yet.": "ロールがありません。",
  "SAVED STEPS": "保存ステップ",
  "Save a step (in its inspector) → drag it onto any canvas.": "ステップを（インスペクタで）保存 → 任意のキャンバスにドラッグ。",
  "(unnamed)": "（無名）",
  "click to edit · drag onto a step": "クリックで編集 · ステップへドラッグ",
  "name this role to drag it": "ドラッグするには名前を付けてください",
  "drag onto the canvas": "キャンバスへドラッグ",
  "remove template": "テンプレートを削除",
  // step form
  Name: "名前",
  Manager: "マネージャー",
  "pick a backend": "バックエンドを選択",
  Persona: "ペルソナ",
  "Behavior / directive": "挙動／指示",
  "self-spawn": "自己生成",
  "ask human": "人に確認",
  Workers: "ワーカー",
  "drag a CLI/role from the palette onto this card": "パレットから CLI／ロールをこのカードへドラッグ",
  "Save as template": "テンプレートとして保存",
  "Delete step": "ステップを削除",
  "Step cards are the blueprint sketch (saved locally), not config.": "ステップカードは設計図のスケッチ（ローカル保存）で、設定ではありません。",
  remove: "削除",
  // workflow form
  "WORKFLOW · base [workflow]": "ワークフロー · base [workflow]",
  "Manager backend": "マネージャーのバックエンド",
  "default backend": "既定のバックエンド",
  "Default worker agents": "既定のワーカーエージェント",
  "None selected uses every available backend except the manager.": "未選択の場合は、マネージャー以外の利用可能なバックエンドをすべて使用します。",
  "Max depth": "最大深さ",
  "Max calls / manager": "最大呼び出し数／マネージャー",
  "use MCP": "MCP を使用",
  "ask-human": "人に確認",
  "BACKEND MODELS": "バックエンドモデル",
  "Empty uses each CLI's own default. Role and --model overrides still take precedence.":
    "空欄では各 CLI の既定モデルを使用します。ロールおよび --model 指定が引き続き優先されます。",
  "CLI default": "CLI の既定値",
  "Refresh models": "モデル候補を更新",
  "Loading models…": "モデル候補を取得中…",
  "{count} choices · {source}": "{count} 件の候補 · {source}",
  "Could not load models: {error}": "モデル候補を取得できません: {error}",
  "No CLI model list; enter an ID manually.": "CLI の一覧取得に未対応です。ID を直接入力してください。",
  "Candidates are suggestions; custom model IDs remain valid.": "候補は入力支援です。カスタムモデル ID も引き続き指定できます。",
  "Saved to config.toml `[workflow]` and `[backends.*].model` on Save.": "保存時に config.toml の `[workflow]` と `[backends.*].model` に書き込まれます。",
  "Backend transport and default models are managed in Settings → Backends. Role models here still override those defaults.":
    "バックエンドの接続方式と既定モデルは「設定 → バックエンド」で管理します。ここで指定したロールモデルが引き続き優先されます。",
  "Saved to config.toml `[workflow]` on Save.": "保存時に config.toml の `[workflow]` に書き込まれます。",
  // plan / flow preview
  "Plan written to config (from your cards and arrows)": "設定に書き込まれる計画（カードと矢印から生成）",
  "No named steps on the canvas yet — nothing will be written.": "名前付きのステップがまだありません — 何も書き込まれません。",
  "Injected into the manager brief as a non-binding suggestion.": "拘束力のない提案として、マネージャーのブリーフに注入されます。",
  // type form
  "WORKFLOW TYPE ·": "ワークフロータイプ ·",
  "Workflow name": "ワークフロー名",
  "Display name (optional)": "表示名（任意）",
  "Strict code review": "厳格なコードレビュー",
  "Brief (manager instruction for this workflow)": "ブリーフ（このワークフローへのマネージャー指示）",
  "Description (when to use)": "説明（いつ使うか）",
  "✨ Generate with AI": "✨ AI で生成",
  "✨ Generating…": "✨ 生成中…",
  "Roles used (none selected = all worker roles)": "使用ロール（未選択＝全ワーカーロール）",
  "Add roles (the cast) first.": "先にロール（キャスト）を追加してください。",
  "Overrides below — empty = inherit base [workflow].": "以下は上書き — 空は base [workflow] を継承。",
  "Manager backend (override)": "マネージャーのバックエンド（上書き）",
  inherit: "継承",
  "Via MCP": "MCP 経由",
  "Ask a human": "人に確認",
  "Delete this workflow": "このワークフローを削除",
  // role form
  "ROLE ·": "ロール ·",
  "Backends (preference order)": "バックエンド（優先順）",
  "Persona (prompt)": "ペルソナ（プロンプト）",
  "Model (optional)": "モデル（任意）",
  "e.g. opus / gpt-5-codex": "例: opus / gpt-5-codex",
  "Candidates use the first backend in the preference order: {backend}.": "候補は優先順位1番目のバックエンド（{backend}）に合わせています。",
  "Remove role": "ロールを削除",
  "Saved to config.toml `[workflow.roles.*]` on Save.": "保存時に config.toml の `[workflow.roles.*]` に書き込まれます。",
  "No roles. Add one → it becomes a `[workflow.roles.*]`.": "ロールがありません。追加すると `[workflow.roles.*]` になります。",
  // generate modal
  "✨ GENERATE": "✨ 生成",
  "Generate a workflow": "ワークフローを生成",
  "Describe the workflow in plain language. An agent drafts the cast (roles) and an illustrative blueprint — nothing is saved until you review and Save.":
    "ワークフローを平易な言葉で説明してください。エージェントがキャスト（ロール）と例示的な設計図を下書きします — レビューして保存するまで何も保存されません。",
  "e.g. A workflow that strictly reviews PRs and hardens security & edge cases with refutation":
    "例: PR を厳密にレビューし、反証でセキュリティとエッジケースを固めるワークフロー",
  "Enter a description.": "説明を入力してください。",
  Cancel: "キャンセル",
  "Generating…": "生成中…",
  Generate: "生成",
  "No backend available to generate.": "生成できるバックエンドがありません。",
  "Invalid generation result.": "生成結果が不正です。",
  // validation
  "Enter a name": "名前を入力してください",
  "Only lowercase letters, digits, - and _ (must start alphanumeric)": "小文字・数字・-・_ のみ（先頭は英数字）",
  "This name is already in use": "この名前は既に使われています",
  "This name is reserved (an agentpit workflow subcommand)": "この名前は予約されています（agentpit workflow のサブコマンド）",
  "(step)": "（ステップ）",
  // tri-state + misc
  on: "オン",
  off: "オフ",
  "drag onto a step": "ステップへドラッグ",
  "Invoke:": "実行:",
  dynamic: "動的",
  WORKFLOW: "ワークフロー",
  "Saved to config.toml `[workflow.types.*]` on Save.": "保存時に config.toml の `[workflow.types.*]` に書き込まれます。",
  // live workflow-run view
  "Workflow run": "ワークフロー実行",
  "{n} stages": "{n} ステージ",
  "{running} running / {total} stages": "{running} 実行中 / {total} ステージ",
  "finished · {n} stages": "完了 · {n} ステージ",
  "{n} failed": "{n} 件失敗",
  "LIVE FEED": "実況",
  "{name} deployed": "{name} 誕生",
  "{name} returned": "{name} 帰還",
  "{name} failed": "{name} 失敗",
  "Follow the latest run": "最新の実行を追う",
  "No workflow has run yet — start one with `agentpit workflow …`": "まだワークフローの実行がありません — `agentpit workflow …` で開始してください。",
  live: "実行中",
  finished: "完了",
  running: "実行中",
  done: "完了",
  failed: "失敗",
  Status: "状態",
  Kind: "種別",
  Role: "ロール",
  Elapsed: "経過",
  Directory: "ディレクトリ",
  "Run ID": "ラン ID",
  Agents: "エージェント",
  "No agent has started yet.": "まだエージェントが開始していません。",
  aggregator: "集約",
  "{n} chars": "{n} 文字",
  "route: {label}": "ルート: {label}",
  "{grade} / rank {rank}": "{grade}点 / {rank}位",
  // learning view
  Learning: "学習状況",
  "{n}% measured": "実測 {n}%",
  "{runs} runs · {labels} labels": "{runs} ラン · {labels} ラベル",
  Refresh: "再読込",
  "Reading the capability matrix…": "能力マトリクスを読み込み中…",
  "Matrix coverage": "マトリクスの実測率",
  measured: "実測",
  cells: "セル",
  seeded: "初期値",
  learned: "学習済み",
  benchmarked: "ベンチ実測",
  "Evidence quality": "根拠の強さ",
  outcome: "人間の判定",
  grade: "採点",
  rerun: "再ディスパッチ",
  exit: "終了コード",
  "No labelled run yet.": "ラベル付きのランがまだありません。",
  "A human verdict outweighs six exit codes. {n} labels total.":
    "人間の判定1件は終了コード6件より重い。合計 {n} ラベル。",
  "Learned policy replay": "学習方針のリプレイ精度",
  "{correct} of {evaluable} evaluable decisions would have gone well":
    "評価可能な {evaluable} 件の判断のうち {correct} 件が良い結果だった",
  "{skipped} of {decisions} decisions had no label for the policy's pick and were skipped.":
    "{decisions} 件のうち {skipped} 件は選択先のラベルが無く評価対象外。",
  "Similarity (kNN)": "類似度ルーティング (kNN)",
  "not in this build": "このビルドには未搭載",
  "disabled in config": "設定で無効",
  "no samples yet": "サンプルなし",
  active: "有効",
  samples: "サンプル",
  "good / bad": "良 / 不良",
  needs: "必要数",
  "This build routes on the profile matrix alone (no --features similarity).":
    "このビルドは能力マトリクスのみでルーティングします（--features similarity なし）。",
  "Capability matrix": "能力マトリクス",
  "colour = score · badge = provenance · dot = has labels":
    "色 = スコア · 記号 = 出所 · 点 = ラベルあり",
  coding: "コーディング",
  refactor: "リファクタ",
  review: "レビュー",
  adversarialreview: "敵対的レビュー",
  securityreview: "セキュリティレビュー",
  debug: "デバッグ",
  explain: "解説",
  docs: "ドキュメント",
  planning: "計画",
  longcontext: "長文コンテキスト",
  "Stored score": "保存されたスコア",
  Confidence: "確信度",
  Samples: "サンプル数",
  "Measured at": "実測日時",
  "No labelled run has touched this cell.": "このセルに触れたラベル付きランはまだありません。",
  Labels: "ラベル",
  "to promote": "で昇格",
  "Would score": "昇格後のスコア",
  confidence: "確信度",
  "Newest label": "最新ラベル",
  "Evidence per cell": "セルごとの根拠",
  "a cell needs {n} labels before the fold writes it":
    "セルは {n} ラベル貯まって初めて書き込まれる",
  "No cell has labelled evidence yet. Dispatch work, then note the outcome.":
    "根拠のあるセルがまだありません。作業をディスパッチし、結果を記録してください。",
  "benchmarked — learned cannot overwrite": "ベンチ実測 — 学習では上書き不可",
  promoted: "昇格済み",
  accruing: "蓄積中",
  "Labels per day": "日次ラベル数",
  "last {n} days · green good / red bad": "直近 {n} 日 · 緑=良 / 赤=不良",
  "No labels in this window.": "この期間のラベルはありません。",
  "Where each category routes": "カテゴリごとの現在のルート",
  "quality margin {n} · available: {list}": "品質マージン {n} · 利用可能: {list}",
  "auto_route is off — dispatch uses the default backend and never consults this matrix.":
    "auto_route が無効 — ディスパッチは既定バックエンドを使い、このマトリクスを参照しません。",
  "[routes] pins {list} — capability routing does not run for those tools.":
    "[routes] が {list} を固定 — 該当ツールでは能力ルーティングが走りません。",
  unrouted: "ルートなし",
  "cost tiebreak": "コスト優先",
  "moved from {backend}": "{backend} から移動",
  "just now": "たった今",
};

export function makeT(lang) {
  return (s, vars) => {
    let out = lang === "ja" && JA[s] != null ? JA[s] : s;
    if (vars) for (const k in vars) out = out.split("{" + k + "}").join(vars[k]);
    return out;
  };
}

// Module-level current translator, so sub-components can call t() without prop
// threading. StudioApp calls setStudioLang(lang) synchronously at the top of its
// render (before children render), keeping every t() in the tree in sync.
let current = makeT(detectLang());
export function setStudioLang(lang) {
  current = makeT(lang);
}
export function t(s, vars) {
  return current(s, vars);
}
