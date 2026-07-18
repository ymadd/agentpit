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
  "Max depth": "最大深さ",
  "Max calls / manager": "最大呼び出し数／マネージャー",
  "use MCP": "MCP を使用",
  "ask-human": "人に確認",
  "BACKEND MODELS": "バックエンドモデル",
  "Empty uses each CLI's own default. Role and --model overrides still take precedence.":
    "空欄では各 CLI の既定モデルを使用します。ロールおよび --model 指定が引き続き優先されます。",
  "CLI default": "CLI の既定値",
  "Saved to config.toml `[workflow]` and `[backends.*].model` on Save.": "保存時に config.toml の `[workflow]` と `[backends.*].model` に書き込まれます。",
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
