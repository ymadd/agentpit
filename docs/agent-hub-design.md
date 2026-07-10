# agentpit — Agent Hub 設計（統合）

> 本書は「タスクに適任のエージェントを割り当てるハブ」化に向けた4本柱の統合設計。
> 各柱はアンサンブル合議・敵対的ディベートで検証済み。実装はこの設計を単一の真実とする。
> ステータス: **設計確定 / 実装はフェーズA（Strataブートストラップ）から**。

## 0. ビジョンと4本柱

異なるバックエンド（claude/codex/gemini/antigravity(agy)/opencode）は得意分野が違う。
agentpit を「能力プロファイルに基づき、タスクに適任のエージェント（群れ）を動的に割り当て、
群れ同士が引き継ぎ・反証しながら協働するハブ」にする。

| 柱 | 内容 | 状態 |
|----|------|------|
| **① 能力プロファイル + タスク診断ルーティング** | backend×TaskCategory のスコア行列で動的ルーティング | 設計確定 |
| **② gold ベンチ課題集** | プロファイルを実測で埋める決定的採点スイート | 設計確定（合議） |
| **③ ボス仲介の階層エスカレーション** | 既存 workflow + guard を拡張、群れが群れを呼ぶ | 設計確定 |
| **④ 群れ間コミュニケーション層** | handoff / clarify / board / 反証、Conductor仲介 | 設計確定（ディベート） |

---

## 1. 能力プロファイル + タスク診断ルーティング

### 1.1 現状

`src/router.rs:61` の `Router::resolve` は完全ルールベース:
`explicit → routes表 → 長文(長さ閾値) → reviewキーワード → default`。
タスクの中身を見るのは文字数とキーワードだけ。ここに能力プロファイル＋診断を足す。

### 1.2 TaskCategory（新規）

診断の出力単位かつプロファイルの列。`RouteKey`（コマンド種別）とは**直交**させる。

```
Coding | Refactor | Review | AdversarialReview | SecurityReview
| Debug | Explain | Docs | Planning | LongContext
```

`LongContext` は「カテゴリ」でなく「特徴量」。閾値超過時のみカテゴリ昇格する。

### 1.3 CapabilityProfile（`src/profile/model.rs`）

```rust
struct CapabilityProfile {
    backend: BackendId,
    scores: BTreeMap<TaskCategory, Score>,  // ベンチ採点由来
    telemetry: TelemetryStats,              // events 由来（将来補正、枠だけ）
    source: ProfileSource,                  // Seeded | Benchmarked | Learned
    measured_at: Option<String>,
}
struct Score { value: u8, samples: u16, confidence: f32 }  // 0–100
```

immutable 方針: 補正は純関数 `apply_benchmark(profile, results) -> CapabilityProfile`。

### 1.4 profiles.toml（`~/.config/agentpit/profiles.toml`）

config.toml と**分離**（手書き設定＝config / 機械生成値＝profiles）。
`[routes]` を機械が上書きする事故を防ぐ。`source` 優先度: `benchmarked > learned > seeded`。

### 1.5 タスク診断（ヒューリスティック → 低confidence時のみLLM）

`src/diagnose/`。
1. **ヒューリスティック層**（純関数, LLMコールなし）: トークン量・コードブロック有無・命令動詞・
   拡張子言及をカテゴリ別重み付きスコア化 → softmax で confidence。
2. **LLM補助層**（confidence < 0.55 のときだけ）: 最速の利用可能 backend に分類専用の短プロンプト1コール。
   失敗・timeout はヒューリスティック結果にフォールバック（診断でブロックさせない）。

`agentpit diagnose "<task>"` は**ドライラン観測点**: features → category(conf) → 選定 backend → 理由 を表示。
`--json` で GitHub Action から消費可能（issue→診断ルーティングは Phase B）。

### 1.6 Router 統合

```
explicit → routes表 → [diagnose → profile argmax(利用可能backend内)] → default
```
- `RouteReason::Profile { category, score }` を追加。既存 `AutoLongContext/AutoKeyword` は将来 deprecate。
- confidence が低く LLM 補助も使えない時は profile 選択せず default 退避（誤分類で変な backend に飛ばさない）。
- `Router::new` に `profiles: ProfileSet` を DI 注入。

### 1.7 コマンド

```
agentpit profile show | seed [--force] | reset | run [opts]
agentpit diagnose "<task>" [--issue <n>] [--json]
```

### 1.8 ファイル構成

```
src/profile/{mod,category,model,store,seed}.rs  src/profile/bench/{mod,suite,judge,merge}.rs
src/diagnose/{mod,features,heuristic,llm}.rs     src/cli/{profile,diagnose}.rs
```

---

## 2. gold ベンチ課題集

アンサンブル（claude/codex/antigravity/gemini ＋ claude統合）で確定。
LLM-as-judge の主観を、可能な所は**機械判定**で置換する。

### 2.1 共通機械判定インフラ

- **Review系は構造化出力を強制**: 末尾で `json` 配列のみ要求 → `serde_json` パース、失敗=スコア0。
- **コード系は sandbox 実行**: 最後の ` ```lang ` フェンス抽出 → 一時 fixture → `pytest`/`cargo test`
  をネットワーク遮断・30sタイムアウトで実行。pytest/cargo 両対応。
- **Review系マッチング**: 報告行が埋込欠陥の ±2行 かつ 種別/CWE一致でヒット、重複は1件に正規化。

### 2.2 確定スイート（完全 gold = 7カテゴリ）

| カテゴリ | 課題 | 決定的判定 | スコア |
|---|---|---|---|
| **Coding** | parse_duration / Top-K頻度 / RLE | 隠しテスト pass数 | `passed/total` |
| **Debug** | 二分探索境界 / 可変デフォルト引数 / inclusive off-by-one(overflow含) | 状態リーク・境界テスト | `passed/total` |
| **Refactor** | 重複一元化 / ネスト平坦化 / O(N²)→O(N) | **振る舞い等価をハードゲート** → 通過時のみ AST(複雑度/行数)・実行時間を評価 | `behavior_pass ? metric_norm : 0` |
| **Review** | APIハンドラ既知バグ / 仕様違反 / **ノイズ耐性(バグ0→正解は[])** | line±2 + kind照合, F1 | `F1` / 過検出は `1/(1+FP)` |
| **SecurityReview** | 注入系5件(CWE付) / 認証・秘密4件 / **偽陽性耐性** | **CWE-id一致** | `F1` / `1/(1+FP)` |
| **AdversarialReview** | 嘘コメント耐性 / 微妙な欠陥+**デコイ** / テスト通過下の仕様違反 | 本物=TP, デコイ指摘=ハードFP(重み2) | **重み付きF1** |
| **LongContext** | needle抽出 / 設定後勝ち解決 / "Lost in the Middle"位置バイアス | exact-match (LLM不要) | `correct/N` |

### 2.3 解決した不一致

1. **Refactor合成式**: 加重和 ではなく **振る舞い等価ハードゲート**（壊したリファクタに部分点を出さない）。
2. **Sec欠陥識別子**: キーワード ではなく **CWE-id一致**（言い換えに非依存）。severity は副次ボーナス。
3. **Adversarial偽陽性対策**: binary検出ではなく **デコイへの指摘に重み2のハードFP**。

### 2.4 LLM-judge 委譲（hybrid 採点 = 3カテゴリ）

**Explain / Docs / Planning**。純LLMでなく「決定的キーワード/被覆率チェック × rubric」で主観分散を縮小:
- Explain: 必須キーワード（関数名・計算量）含有率 × rubric
- Docs: docstring パラメータ被覆率 × rubric
- Planning: 必須サブタスクID被覆率 × rubric

judge LLM 呼び出しはこの3カテゴリのみに削減できる。

---

## 3. ボス仲介の階層エスカレーション

### 3.1 既存資産

`agentpit workflow`（`src/cli/workflow.rs`）= manager(claude/codex) がゴールを分解し各サブタスクを
best fit な worker に動的ディスパッチ（`rescue`/`ensemble`）。静的DAGなし。
`src/workflow/guard.rs` = `ENV_DEPTH` を子へインクリメント継承、`MAX_DEPTH_CEILING=32` +
`check_not_exceeded` で fan-out 暴走を Rust 側で強制停止。worker は sub-boss になれる構造が既にある。

### 3.2 差分（3点）

1. **プロファイル注入** — manager プロンプトに能力行列を差し込み「best fit」を勘から事実へ
   （+任意で各サブタスクに `agentpit diagnose` を前段実行）。
2. **第3のディスパッチ動詞 `workflow`** — plain worker(`rescue`)・並列(`ensemble`)に加え、
   サブタスクが多段 or 適任不確実なら **sub-boss へ委譲**。sub-boss が自分で診断して再ルーティング。
   深さガードが上限を保証。
3. **エスカレーション規律（human規律の横展開）** — `src/ask` の「workers は人間を呼べない、
   ボスだけが仲介する」をそのまま転用。worker は勝手に群れを呼ばず、末尾に構造化シグナル
   `ESCALATE: {category, reason, evidence}` を返す。**ボスがそれを読み、適任の専門群れ（=サブworkflow）へ再ディスパッチ**。

これで `review`/`security-review`/`adversarial-review` が独立コマンドでなく
「ボスが状況に応じて召喚できるレンズ（専門群れ）」に昇格する。

### 3.3 必須ガード

| リスク | 既存 | 追加 |
|---|---|---|
| 深さ爆発 | ✅ `MAX_DEPTH_CEILING` | — |
| 幅爆発 | △ `max_calls_per_manager` | 段あたり幅上限 |
| ループ(A→B→A) | ❌ | 訪問済みカテゴリ集合 / 同一カテゴリ再昇格禁止 |
| コスト爆発 | ❌ | workflow全体の累積トークン/コール予算（env継承） |
| 重複発見 | ❌ | ボス側 findings dedup |

### 3.4 ロール（キャスティング設定）

**ロールが固定するのは CAST であって SCRIPT ではない**: manager は相変わらず分解・順序を
その場で即興する。固定したいのは「どのバックエンドがどのペルソナを演じるか」だけで、
それを LLM の気分から config に移す（`src/workflow/roles.rs`）。実装済みの経路は3本:
(1) 呼び出し側の明示ディスパッチ `agentpit rescue --role <name>`、(2) `agentpit workflow`
の manager が受け取るロール名ロスター（ワーカーロールが1つでもあれば AVAILABLE ROLES +
`rescue --role` / `dispatch_task {"role"}` 文法に切替わる）、(3) MCP `dispatch_task` の
`role` 引数。manager 自身の解決順にも `roles.manager` が組み込まれている（下記）。

#### スキーマ（`[workflow.roles.<name>]`）

```toml
[workflow.roles.<name>]
backends = ["claude", "codex"]   # 優先順（先頭から利用可能なものが勝つ）。空 = 任意
prompt   = "…persona…"           # ディスパッチのたびに前置されるペルソナ文（任意）
```

解決ロジックは `converse::pick` と同じ「優先順 → 決定的な利用可能フォールバック」形を踏襲し、
バックエンド選定を再現可能に保つ。

#### 予約ロール `manager` と resolve_manager

`manager` という名前はワーカーではなく**オーケストレータ自身**を設定する予約ロールで、
`workflow::roles::resolve_manager` が解決ロジックを持つ:

```
[workflow.roles.manager].backends の先頭 SUPPORTED（claude|codex）項目
  > backends が空なら None（persona だけが乗り、バックエンド選定は呼び出し側に委ねる）
```

`manager` ロールへ `rescue --role manager` でディスパッチするのは `resolve_role`側で
ハードエラーになる — オーケストレータ自身をワーカーとして呼び出すのは意味がない
（`resolve_role` は名前が `manager` なら即 bail する）。

#### workflow への配線（実装済み）

`agentpit workflow`（`src/cli/workflow.rs`）の manager バックエンド解決順は:

```
--manager フラグ > [workflow.roles.manager]（claude|codex の先頭）
  > [workflow].manager_backend > [default].backend
```

manager ロールの persona は、バックエンドがどの段で決まったかに関わらず（persona-only の
manager ロールでも）オーケストレータプロンプトに `MANAGER PERSONA` ブロックとして注入
される。ワーカーロールが1つでも設定されていれば、manager プロンプトのロスターは
`AVAILABLE ROLES:`（`<name> (<解決済みbackend>): <persona 1行要約>`）に切替わり、
ディスパッチ文法も shell モードは `rescue --role <name>`、MCP モードは
`dispatch_task {"role":"<name>", ...}` を教える。解決できないロールは warning 付きで
ロスターから除外され、**全ワーカーロールが解決不能なら起動時にハードエラー**。roles 設定
時に `--agents` を渡すと warning の上で無視される（roles が勝つ）。

#### ゼロロール = 完全後方互換

`[workflow.roles.*]` が一つも無ければ manager プロンプトは従来のフラット backend ロスター
のまま **バイト単位で同一**（`legacy_prompt_is_byte_identical_without_roles` /
`legacy_mcp_prompt_is_byte_identical_without_roles` がゴールデン文字列でピン）。既存
ユーザーの `agentpit workflow` 挙動は一切変わらない。

#### ワーカーディスパッチ文法

```
CLI:  agentpit rescue --role <name> "<sub-task>"
MCP:  mcp__agentpit__dispatch_task {"role":"<name>","task":"<sub-task>"}
```

`--role` と `--backend`（MCP では `role` と `backend`）は排他 — 両方/どちらも無しは
構造化エラー。`role_name` は `[workflow.roles.<name>]`（`manager` を除く）に対して解決
され、persona がタスクに前置された上で解決先バックエンドへディスパッチされる。dashboard
のイベントは解決済み backend 名で記録されるので swarm 表示は従来と同形。

#### 解決セマンティクス

1. 優先順リスト（`backends`）を先頭から走査し、**現在利用可能**な最初のバックエンドを採用
   （利用可能性より config の優先順が勝つ）。
2. `backends` が空なら、利用可能集合をソートした先頭（`converse::pick` と同じ決定的
   フォールバック）。
3. 未知のロール名、および利用可能なバックエンドが1つも無いロールは**ハードエラー**
   （黙って別 backend に差し替えると「config でキャスティングを固定する」意味が消える）。

#### dashboard Settings パネル

`[workflow.roles.*]` はデスクトップ dashboard の Settings パネルからも編集できる
（`dashboard/src-tauri/src/settings.rs`）。書き込みは `toml_edit` 経由（`toml::to_string`
によるファイル全体の再シリアライズではない）でコメントと整形を保持したまま該当キーだけを
更新する — ユーザーが手書きした他セクションのコメントを潰さない。ロール名は
`^[a-z0-9][a-z0-9_-]*$` にバリデートされ、重複名は拒否される。

---

## 4. 群れ間コミュニケーション層

### 4.1 ディベート判定: **ADOPT WITH MODIFICATIONS**（confidence: high）

4立場（PRO / CON-ACP / CON-MINIMAL / CON-BUS）× 敵対的反証 → opus moderator の統合判定。
アーキ骨格は反証を生き残り、字義どおりの2機構が論破され、④が補強された。

### 4.2 決定的制約

exec backend（claude/codex/gemini/agy）は結果を返すと**プロセス終了**（`dispatch.rs:78`
「ACP transport is wired only for opencode」）。生きた双方向チャットは両端の生存が必要で、
常駐セッションは現状 opencode の ACP のみ。→ 生きた peer-to-peer は構造的に不可。

### 4.3 維持（全角度から生存・ファイル検証済み）

1. **worker は stateless ワンショットのまま**（exec 境界による強制、スタイルでない）。
2. **「Conductor」= 新規ステートフル仲介者ではなく既存の workflow manager**。
   状態所有型 Conductor は manager より重い SPOF。
3. **`src/ask` は不変・human専用のまま**。隔離ゲート（`exec/base.rs:62` の `env_remove(ENV_ASK_ALLOWED)`
   vs `workflow.rs:178` の manager限定セット）は worker が ask チャネルに入らないからこそ意味を持つ。
4. **④ は `guard.rs`（MAX_DEPTH_CEILING=32, check_not_exceeded）で停止保証済み**。
   常駐 opencode-only ACP 対話は default-NO。

### 4.4 変更（字義が論破された）

| # | 命題の字義 | 訂正 | 根拠 |
|---|---|---|---|
| (a) | 「ask を宛先付きバスに一般化」 | **durable な①③は `events.jsonl` に置く**。ask sidecar は GC免除・保持なし（`agentpit-events/src/lib.rs:237`）、events.jsonl は append-only・順序付き・compaction-bounded（`COMPACT_KEEP_RUNS=500`, lib.rs:356/575）で**形が真逆** | CON-BUS、最重要訂正 |
| (b) | 「②= ask の宛先を人間→群れに拡張」 | **②は単なる `dispatch_task`**（sub-swarm は exec可能で文字列を返す）。これで multi-responder 競合も隔離 must-fix も同時消滅 | 全立場合意 |
| (c) | 「④= 1ショット反証 fan-out」 | **④は ≥3レグ critique→defense→adjudication**。1ショット懐疑バイアス批判（`adversarial_review.rs`「assume broken」）を無反論で中継すると詰まった群れを悪化させる。常駐セッション不要 — 各レグは transcript から前ターンを読む逐次ワンショット | CON-ACP の唯一の生存点 |

### 4.5 ビルド順（会話層M1）

```
DO（今マイルストーン）
 1. events.jsonl に Event variant を1つ追加（Note/BoardPost、宛先フィールドもカーソルも無し
    — 長命 consumer は manager 1つだけ）。append_line + compaction 再利用。①③の土台
 2. ①handoff を最初に実装 — 最もクリーンな意味論（durable/順序/1→1/撃ちっぱなし/claim-ack不要）
 3. ②= dispatch_task として実装（一般化 ask にしない）
 4. ④= 深さガード付き3レグ dispatch（critique=adversarial-review → defense=批判を載せ再dispatch
    → adjudication=manager）。各レグが guard.rs + dispatch timeout を継承。新規 state ゼロ
 5. 上記 + 「詰まった sub-task を捨てる前に critique→defense→adjudication を1回」を
    src/cli/workflow.rs の PROCEDURE 文として明文化

DO NOT
 - src/ask を一般化しない／response sidecar に宛先を足さない（単一 <ask_id>.response.json への競合）
 - exec/base.rs:62 の隔離 env_remove を弱めない／常駐 Conductor クラスを作らない
 - 宛先付きバス / claim-ack キュー / per-consumer カーソルを今作らない（第2の長命参照先が無い）
 - 常駐 opencode↔opencode ACP 反証ループを今作らない

DEFER（gold-bench の収束タスクでゲート）
 - 宛先/topic フィールド + カーソル … 第2の長命 reader が出現したら
 - 常駐 ACP ループ … stateless 3レグが収束品質で劣ると bench が示したら
```

> **再評価（2026-07-01, A4 refute-bench データ後）**: 両項目とも**据え置き継続**。
> - 宛先/topic + カーソル: `Event::Note` の唯一の長命 reader は今も manager のみ
>   （`agentpit-events/src/lib.rs:209-216` のdoc comment通り）。`note.rs`/`post_note`は書き手のみ、
>   dashboard側 (`dashboard/src-tauri/src/state.rs:229-233`) は `Event::Note` を no-op 表示専用で
>   消費しランビューに反映しない。第2の長命 reader はまだ出現していない（§4.6 crux #3 も未決着のまま）。
> - 常駐 ACPループ: `agentpit refute-bench` のGATE: PASSは**exec-transport**（codex/antigravity、
>   ACPではない）の stateless 3レグで出た結果。これは「stateless 3レグが収束品質で劣る」という
>   un-defer条件の**逆方向**の実証であり、常駐ループを今作る根拠にならない。`dispatch.rs:77-78`の
>   ACP配線も opencode のみのまま変化なし。

public surface 破壊なし: `agentpit ask` と `ask_human` MCP は human専用のまま。すべて加算的。

### 4.6 未解決の経験的問い（cruxes — gold-bench 行き）

1. ~~statelessでの**反証品質**~~ — **決着（2026-07-01, `agentpit refute-bench`）**: critic=codex /
   defender=antigravity で3probe（binary_search_bounds / mutable_default_arg / parse_duration）
   全て before=0.00 → after=1.00（delta margin 0.20 を全probe通過、GATE: PASS）。プロンプト注入
   再dispatchは劣化版ではなく本物の defense ターンとして機能した。`docs/agent-hub-design.md`
   §5.1 の境目はこれで実証済み。3probeのみ・1backend-pairのみなので一般化の余地は残る
   （複数backend-pair・敵対的に難しいprobeでの再測定は今後の回帰ゲート運用に委ねる）。
2. Event variant を**今作るか dogfood 後か**（形は合意、YAGNI の時期だけ対立）。
3. durable transcript に明示的 recipient/cursor が**いつか要るか**（群れトポロジが1manager×多leafのままか）。
4. ④の adjudication 裁定者は **manager 自身か別dispatchの中立群れか**（manager は単一 --print/exec 窓）。

---

## 5. ビルド計画とドッグフーディングの境目

### 5.1 ドッグフーディングの境目（決定）

> **境目 = ④反証（3レグ critique→defense→adjudication）が出荷され、gold-bench がそれをグリーンで検証した瞬間。**

理由: ④以前の agentpit は詰まった時に自己修正できない。自己修正できないツールで自分を作るのは
ディベートが最も警告した失敗モードそのもの。④（自己修正）＋ gold-bench（品質測定）が揃うと
「能力で振り分け・文脈引き継ぎ・反証で立ち直り・出力品質を測れる」の最小条件が成立し、
gold-bench が回帰ゲートとなりドッグフーディングが安全になる。境目は測定可能な1線（bench green on ④）。

> **境目通過（2026-07-01）**: `agentpit refute-bench` 実装 + ライブ実行で GATE: PASS（§4.6 crux #1
> 参照）。Phase B 着手可。

### 5.2 フェーズ

```
Phase A — Strata ブートストラップ（外部足場が "自己ビルダー" を作る）
  A0  本統合設計ドキュメント            … solo 統合
  A1  能力プロファイル + 診断ルーティング  … Strata ultra/conduct
  A2  gold-bench ハーネス + profile run   … Strata conduct
  A3  会話層M1（§4.5 の DO）             … Strata conduct（敵対的 verify 必須）
  A4  ④反証品質gold-benchゲート（§5.1）  … 通過済み（2026-07-01, GATE: PASS）
 ───────────────◀ 境目: gold-bench が ④反証でグリーン ─── 通過済み ───────
Phase B — ドッグフーディング（agentpit が agentpit を作る）
  B1+ DEFER項目、hybrid-judge カテゴリ(Explain/Docs/Planning)、issue→診断ルーティングの
      GitHub Action、events からの継続学習テレメトリ … 全部 `agentpit workflow`(①+④) で駆動、
      gold-bench を回帰ゲートに、Strata は時々の敵対的レビュアーとしてのみ
```

### 5.3 Phase A 内の Strata モード指針

- A1/A3 = 複数ファイルに割れる実装 → `conduct`（file-disjoint 並列）または `ultra`（探索が要る所）。
- A2 = ハーネス実装 → `conduct`。
- A0 = 既決定の統合 → **solo**（Strata GATE: 名指しできる単一統合は SOLO）。
