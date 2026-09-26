# agentpit — ワークスペース・ループ設計（ブループリント + ループ実行 + 状態/イベント/操作スキーマ）

> **目的**: 人間と AI がグラフとして設計した**ブループリント**を、agentpit が**有界ループ**として自律実行する。「どのループが・どの段階で・どのエージェントに渡っているか」を問い合わせ（状態）、流し（イベント）、操作（開始・停止・承認・範囲指定の指示・差分採用）できる、**画面を持たないコア**にする。画面は既存の Tauri アプリ（`dashboard/`）。
> 本書はオーナーの進め方の**ステップ1「状態とイベントのスキーマを決める」**の成果物であり、ステップ2〜5の段取りと受け入れ基準も定める。
> **スコープ**:
> - オブジェクトモデル、ブループリント言語と実行意味論
> - 状態・イベント・操作スキーマ、状態機械
> - 永続化（append-only JSONL ジャーナル）、トランスポート（既存 UDS NDJSON）、互換性ルール
> - 既存原則の**改訂案**、ACP の位置づけ、型の置き場所、実装フェーズ
> **非目標**:
> - ブラウザ／HTTP／TCP／Tailscale での提供（**オーナー決定 2026-09-25**。UI は Tauri アプリのみ）
> - 新しい UI シェル（**オーナー決定 2026-09-25**。既存の Tauri アプリを拡張する）
> - 汎用ワークフロー言語（式言語・任意サイクル・実行中のノード生成）
> - `agentpit workflow`（manager 経路）の置き換え。manager 経路はそのまま残す
> **ステータス: P1（スキーマ実装）完了（2026-09-25）。§14 の原則改訂はオーナー承認済み（Q1、2026-09-26）。P2（デーモンの窓口）実装済み（2026-09-26、§17 P2 の「実装メモ」）。P3（画面での確認と実行）実装済み（2026-09-26、§17 P3 の「実装メモ」）**。
> 実装: `agentpit-events/src/loops/`（`mod.rs` `blueprint.rs` `record.rs` `state.rs` `ops.rs` `journal.rs`）、固定テスト `agentpit-events/tests/loops_golden.rs` と `tests/fixtures/loops/`。P2: `agentpit-events/src/wire.rs`、`src/loops/`（`sched.rs` `records.rs` `runner.rs` `effects.rs` `classify.rs` `prompt.rs` `proc.rs` `control.rs` `paths.rs`）、`src/cli/loop_cmd.rs`、`tests/loop_e2e.rs`。P3: `agentpit-events/src/loops/`（`view.rs` `store.rs` `files.rs`）、`dashboard/src-tauri/src/bridge/`、`dashboard/frontend/src/loops/`。
> **根拠**:
> - agentpit 現状調査（2026-09-25、9 サブシステムのコード読解。主要主張 14 点は file:line で再検証済み）
> - 独立 4 設計案（耐久性優先／UI 優先／ループ意味論優先／相互運用優先）と 3 審査、統合案への 3 方向の敵対的検証
> - serde 挙動の実測（`#[serde(other)]` の flatten・内部タグ時の挙動、`large_enum_variant`）

## 0. 現状調査の要約（設計の前提）

| 観点 | agentpit の現状 | 設計への含意 |
|------|----------------|--------------|
| 問い合わせ・配信・操作の窓口 | **会話セッション単位でのみ既に存在**。List/Status、Attach と Chunk/TurnStarted/TurnFinished/Notice の push、Send/Cancel/Branch/Fork/Compact/ReplCell（`src/daemon/protocol.rs:17-206`）。状態は busy フラグから都度計算（`server.rs:225-249`） | 窓口を作り直すのではない。**セッションからループへの再スコープ**と、**カーソルの追加**が仕事 |
| ループ／ワークフロー | デーモンの外（前景の CLI/MCP）で走る。外から止めることも承認することもできない（MCP の `run_workflow` は新規トークン、`mcp/tools.rs:645`）。manager LLM が即興する設計（`cli/workflow.rs:6`「no static DAG」） | ループを**デーモン配下のオブジェクト**にする。静的 DAG の禁止は**改訂**が要る（§14） |
| 既存のループ的基盤 | `rescue --cascade`（有界のラダー + verify 述語。ただし毎ホップ同じタスクで、ロールバックもない。`cli/rescue.rs:433-671`）、REPL セル（ホストコールは直列）、refute（1往復）、arena（並列 worktree + 投票） | cascade の verify を check ノードとして、arena の worktree/patch を提案として再利用する。失敗フィードバックの受け渡しは新設する |
| ブループリント | Studio の React Flow キャンバス。保存先は localStorage だけ。保存時に DFS で線形化され、`[[workflow.steps]]` の**ヒント**になる（`studio/blueprint.js:114-133`）。分岐とサイクルはここで失われる | **実行可能なブループリント**を別の成果物として新設する。steps は manager へのヒントとして残す |
| カーソル／seq | events.jsonl にもデーモンの Event にも seq・エンベロープ・ts が**ない**。events.jsonl はロックも fsync もなく、run_id のない行は compaction で消える（`agentpit-events/src/lib.rs:566-653`） | ループの事実は**新しい seq 付きジャーナル**に置く。events.jsonl に載せるのはテレメトリだけ |
| events.jsonl の役割 | テレメトリであると同時に**ルーティングの入力**。可用性のサスペンド（`availability.rs:95-117`）と学習（`profile/learn.rs:10-50`。OutcomeNoted の重みは 3.0、6h 以内に同じ task_hash を別バックエンドへ再送すると失敗ラベル） | ループの失敗で MemberFinished Error を汚さない（cascade の Skipped の先例に従う）。承認・却下は OutcomeNoted に写す |
| 差分レビュー | 通常の経路にはない。全バックエンドがフル自律で cwd を直接編集する。StreamDecoder は Edit/Write をパスだけに縮約する（`exec/stream.rs:470-471`）。「提案→採用」は arena だけ（`arena/worktree.rs:66-135`、`cli/arena.rs:257-310`。HEAD 起点で、採用は CLI からのみ） | 差分レビューには、先に**実行モデルの変更（worktree による隔離）**が要る。フェーズ4 |
| セッション JSONL | 単一ライター（mkdir リース）の append-only ツリー。exchange/result の対がクラッシュジャーナルを兼ねる（`session.rs:66-242`） | 「開始→終了の対。終了の欠落はクラッシュの証拠」を step_started/step_finished に一般化する。リース（`session_lease.rs`）はそのまま再利用する |
| 互換性の弱点 | `#[serde(other)]` がどこにもない。新しい variant や新しい BackendId があると、旧リーダではその**行ごと消える**。未知の verb には id 0 で応答し、旧クライアントがハングする（`server.rs:151`） | 新しいスキーマは二段デコードにし、全 enum に Unknown を持たせる。critical/ancillary を区別する（§13） |
| Tauri ダッシュボード | デーモンとは話さず、events.jsonl を notify で畳み込む（`dashboard/src-tauri/src/main.rs:182-221`）。依存は `agentpit-events` だけ | 共有する型は **agentpit-events** に置く。ブリッジは `dashboard/src-tauri` が UDS で話し、invoke/emit に橋渡しする |
| ACP | agentpit は opencode 向けの ACP **クライアント**だけ。Diff/ToolCall/Plan を捨て、AllowOnce を自動で選ぶ（`src/acp/base.rs:25-122`）。JSON-RPC は内部 IPC として却下済み（session-persistence §5.1） | エディタ連携は **ACP のデータ形（Position/Range/Diff/permission）に合わせる**。内部は NDJSON のまま。外部エディタ向けは任意のエッジアダプタ |

## 1. 全体像

```
Tauri webview（React islands: Board / Canvas(設計・実行) / Inbox / Diff / Editor / Artifacts）
        │ invoke / emit（既存の window.__TAURI__ シーム）
dashboard/src-tauri ── bridge（UDS NDJSON クライアント。agentpit_events::loops の同じ fold で畳む）
        │ UDS NDJSON、PROTO_VERSION=1 + hello.features
 ┌──────┼────────────────────────────┬─────────────────────────────┐
 ▼      ▼                            ▼                             ▼
daemon.sock（制御面・状態なし）   loop-<32hex>.sock（1ループ=1ランナー）  worker-<sid>.sock（既存・不変）
 loop_start / ensure / list       ランナー = loops/<id>/journal.jsonl の唯一のライター
 （head.json と registry を読む）  スケジューラ + エフェクト（dispatch_continuing、
 watch（P3、要約のみ）             sh -c の check、gate、repeat）。attach は再生 + ライブ
```

- **語彙**:
  - **ブループリント** = 設計時の版付きグラフ文書。
  - **ループ（インスタンス）** = 凍結したブループリント1版の実行。ジャーナル1本。
  - **ノード** = 段（agent / check / gate / repeat）。
  - **ノードインスタンス** = (node, iter)。「段階」にあたる。
  - **ステップ** = インスタンスの試行。リトライは新しいステップ。
  - **イテレーション** = repeat の1周。
  - **担当（assignee）** = role / backend / model / effort / transport / route。
  - **ゲート** = 人間の決定点。
  - **指示** = 実行中のループに人間が渡すテキスト（P4 から範囲の文脈つき）。
  - **提案** = 差分（P4）。**成果物** = pptx など（P5）。
- **中心原則**:
  - 状態 = **fold(ジャーナル)**。ランナーは**状態を所有しない、再起動可能な実行器**。
  - 全レコードは **fsync してから**可視化する。
  - エフェクトは、それを完全に記述する認可レコードが耐久化してから開始する。

## 2. 主要な設計判断

| # | 争点 | 判断 | 理由 |
|---|------|------|------|
| 1 | 画面とトランスポート | 既存 Tauri アプリ + 既存 UDS NDJSON。HTTP/TCP は足さない | オーナー決定（ブラウザ非対応）。TCP 不採用（session-persistence §5.1）もそのまま守れる。認証層が要らない |
| 2 | ループ言語 | **agent / check / gate / repeat の4種**と、outcome で分岐するエッジ。ループは構造化された `repeat` でだけ書ける。任意のサイクルは書けない | React Flow の parentId・エッジに直写像できる。停止性が静的に言える（§4.4） |
| 3 | repeat の周回決定 | 本体に「どのエッジも処理しない非 ok」が残れば continue、残らなければ break、最終周の continue は exhausted | 「テストが通るまで繰り返す」を追加の構文なしで書ける。`@break` などの疑似ターゲットは v1 に入れない |
| 4 | 実行の骨格と中身 | ブループリントが固定するのは**骨格**（段・順序・分岐・有界反復・ゲート・予算）。ノードの中身は既存のモデル主導の仕組み（ロール、ルータ。P3 で manager ノード）に任せる | CAST not SCRIPT はノードの内側で生き続ける（§14.2） |
| 5 | 状態の持ち方 | ジャーナルを唯一の真実とし、状態はその fold。ランナーは fold と純関数のスケジューラだけを持つ | kill -9 の後も再生で同じ状態に戻る。状態所有型 Conductor の SPOF 懸念に答える（§14.3） |
| 6 | fold と検査の分離 | 寛容な `apply`（全リーダ共通、決してパニックしない）と、厳格な `admit`（ライターが追記前に確認する遷移表） | 古い／変なジャーナルでも表示でき、壊れた履歴は書かない |
| 7 | 耐久性 | バッチ単位で 1 write + 1 sync_data。**fsync 前に配信しない**。選択的な fsync はしない | fsync していないレコードを配信すると、停電後に同じ seq が再発行され、seq カーソルでは検出できない |
| 8 | カーソル | クライアントが `(uid, seq)` を持つ。サーバはクライアントごとの状態を持たない | uid で「同じ loop_id で削除→再作成」を検出する |
| 9 | 前方互換の書き込み保護 | critical/ancillary の区別。解釈できない critical レコード、状態を駆動する未知の値、自分より新しい schema_minor のライター、実行できない凍結ブループリントのどれかがあれば **read-only** | 新しいランナーが書いた後に古いランナーが続けると、誤った fold の上で書いてしまう。この降格破損を安く防ぐ |
| 10 | 凍結ブループリントの再検証 | **作成時だけ** `validate` する。再生時には「理解できるか」（`is_understood`）だけを見る | 後の版で検証を厳しくしても、実行中のループを取り残さない |
| 11 | 回復時の一時停止 | recovery ゲートは**ループを pause しない**。スケジューラが「recovery ゲートが開いている間は新しい計算を始めない」だけ | pause（オペレータ操作）と回復が同じ状態を奪い合わない |
| 12 | リトライの対象 | agent/check だけ。gate と repeat のステップはリトライしない | repeat のリトライは子インスタンスの鍵と衝突する |
| 13 | ジャーナルの作成 | 一時ファイルに最初のバッチを書いて fsync し、rename する | 作成途中のクラッシュで空や途中のジャーナルが残らない |
| 14 | 末尾の切り詰め | 書けると判定した**後**、最初の追記の直前にだけ切り詰める | read-only で開いたときはファイルに触れない |
| 15 | ブループリント保存 | **JSON ファイル**。プロジェクト `.agentpit/blueprints/<name>.json`（git でレビューできる）とユーザ `~/.config/agentpit/blueprints/`。**生の JSON が正** | 往復で未知のキーが残る。config.toml を書き換えない |
| 16 | ボード／ロスター | ランナーが書く **head.json（LoopSummary）**をデーモンが読む（P2） | 退避中のループも受信箱に出る。プローブの遅延がない |

## 3. オブジェクトモデル

```
Blueprint(name, scope, rev) ──凍結コピー──▶ Loop(lp-…) ─ journal.jsonl（seq 1..N）
  └ Node{agent|check|gate|repeat}          ├─* NodeInstance(node, iter)  …「段階」
      └ repeat の子 = ループ本体             │     └─* Step(attempt) ── Assignee, run_id ─▶ events.jsonl の子 run
  └ Edge(from→to, on)                      ├─* RepeatRun(iteration n, decision, feedback)
  └ layout（UI 専用）                       ├─* Gate(g<n>: approval|step_error|budget|recovery)
                                           ├─* Instruction(in<n>)（P4 で EditorContext 付き）
                                           ├─* Proposal(p<n>) ─* File ─* Hunk   …P4
                                           └─* Artifact(a<n>) ─* Preview         …P5
```

- **ID**:
  - `loop_id = lp-<32hex>`。`loop_start` の op_id（正規の小文字ハイフン区切り UUID）から導出するので、開始のリトライは冪等になる。
  - `step_id = <node>[.i<iter を - で連結>].a<attempt>`（例: `implement.i1-2.a1`）。決定的で、ファイル名として安全。
  - `gate_id = g<n>`、`instruction_id = in<n>`（ループ内の連番）。
  - `op_id` はクライアントが生成する（UUIDv7 推奨）。
- **担当（Assignee）** は文字列で持ち、`BackendId` にしない。新しいビルドが足したバックエンドで、行がパース不能にならないようにするため。
- **「どのループが・どの段階で・どのエージェントに」** は `LoopSummary` の1行で答える。
  - 段階 = `active[].node` と `iter`、および `iterations[]`（repeat の n/max）。
  - エージェント = `active[].assignee`（role/backend/model/effort）と `run_id`。
  - 人間待ち = `waiting` と `open_gates[]`。

## 4. ブループリント（人間が読めて編集できるグラフ）

### 4.1 例（`.agentpit/blueprints/fix-until-green.json`）

```json
{
  "schema": "agentpit.blueprint/1",
  "name": "fix-until-green",
  "title": "計画→（実装→テスト）を緑まで→サインオフ",
  "inputs": {"goal": {"required": true}},
  "workspace": {"mode": "in_place"},
  "budget": {"max_steps": 12, "max_active_secs": 7200, "max_parallel": 1},
  "policy": {"on_error": "gate", "on_budget": "gate"},
  "nodes": [
    {"id": "plan", "kind": "agent", "role": "planner", "access": "read", "task": "Goal: {{goal}}\n番号付きの計画を書く。編集しない。"},
    {"id": "fix", "kind": "repeat", "title": "テストが通るまで", "max_iterations": 4, "feedback": "last"},
    {"id": "implement", "kind": "agent", "parent": "fix", "role": "coder",
     "task": "Goal: {{goal}}\n計画:\n{{nodes.plan.output}}\n反復 {{iteration}}\n{{feedback}}\n{{instructions}}"},
    {"id": "test", "kind": "check", "parent": "fix", "command": "cargo test -q", "timeout_secs": 900},
    {"id": "signoff", "kind": "gate", "prompt": "結果: {{nodes.fix.outcome}}。採用しますか？"}
  ],
  "edges": [
    {"from": "plan", "to": "fix"},
    {"from": "implement", "to": "test"},
    {"from": "fix", "to": "signoff"},
    {"from": "fix", "to": "signoff", "on": "exhausted"}
  ],
  "layout": {"plan": {"x": 40, "y": 80}}
}
```

- `test` が失敗すると、それを処理するエッジがないので「未処理の非 ok」が残り、次の周へ進む（最大4周）。
- 通れば break して `fix` は ok になり、`signoff` へ進む。4周とも失敗すると `fix` は exhausted になり、同じく `signoff` へ進む。
- 固定テスト用の同じ文書が `agentpit-events/tests/fixtures/loops/blueprint_fix_until_green.json` にある（`worst_steps = 9`、rev は `b1-6c29e826fc3c2a74` で固定）。

### 4.2 ノード種別（v1）

| kind | 目的 | 再利用する既存部品 | outcome |
|---|---|---|---|
| `agent` | 1回のステートレスな dispatch。担当は role / backend（+model, effort）/ ルータ（category ヒント）のいずれか。`verdict: true` なら最終行の `VERDICT: PASS\|FAIL` を読む | `roles::resolve_role`、ルータ、`dispatch_continuing`、RunLogger | ok / fail（verdict=FAIL）/ error / timeout / cancelled |
| `check` | 決定的な述語（`sh -c`、exit 0 = ok）。コマンドは**テンプレート化しない** | arena の verify と cascade_verify を統合する（P2） | ok / fail / error / timeout / cancelled |
| `gate` | 人間の承認。選択肢ごとに outcome（ok/fail）を持つ。既定は approve(ok) / reject(fail) | ジャーナルに裏付けられたゲート（src/ask は変えない。ミラーは任意） | ok / fail / timeout / cancelled |
| `repeat` | 子を1周ずつ回す有界ループ（最大 20 周、入れ子は 3 段まで） | スケジューラのみ | ok（break）/ exhausted / cancelled |

- `manager`（P3 で実装）: `run_capture` を1ノードの中で即興させる。ステップはジャーナル上 `agent` として記録し、outcome も agent と同じ（§17 P3 の実装メモ）。
- 予約名（`Unsupported` としてパースはでき、保存・編集もできるが、実行はできない）: `ensemble`、`arena`、`apply`。
- エッジの `on` は `ok|fail|error|timeout|exhausted|not_ok|always`（省略時は `ok`）。種別ごとの許可表:

| 起点の kind | 使える `on` |
|---|---|
| agent / check | ok, fail, error, timeout, not_ok, always |
| gate | ok, fail, timeout, not_ok, always |
| repeat | ok, exhausted, not_ok, always |

- `cancelled` はどのエッジでも拾えない。
- `requires`（任意）: 文書が必要とする追加機能の名前の配列。v1 で対応する機能はない。新しい版が「旧版では黙って無視されるキー」に意味を持たせるときは、ここに機能名を書く。そうすると旧版はその文書を拒否する（§13 C8）。

### 4.3 実行意味論（規範。P2 のスケジューラが実装する）

1. **スコープ**: トップレベルと、repeat の各イテレーションがそれぞれスコープになる。インスタンス = (node, iter)。iter は囲む repeat の周回番号を外側から並べたもの。
2. **インスタンス**: インスタンスは `step_started`（attempt 1）か `node_skipped` のどちらかで、一度だけ決まる。リトライは attempt+1 の新しいステップ。
3. **準備完了（AND 結合 + デッドパス除去）**:
   - 入エッジがすべて確定（FIRED か DEAD）し、かつ1本以上が FIRED なら実行する。
   - 全部 DEAD なら `node_skipped{dead_path}` にする。
   - 入エッジのないノードは、スコープの開始時に準備完了になる。
   - 複数が同時に準備完了なら、ブループリントでの宣言順に始める。
4. **未処理の非 ok**: 実効 outcome が ok 以外で、どの出エッジにも合致しないもの。
   - 発生したスコープは早めに終わる。未決のインスタンスは `node_skipped{scope_ended}`、実行中のステップは `cancel: scope_ended` で取り消す。
   - トップレベルで発生した場合 → `loop_finished{failed, unhandled_outcome, node}`。
5. **反復の決定**: スコープが落ち着いた（実行中のステップも準備完了のインスタンスもない）とき、未処理の非 ok があれば `continue`、なければ `break`。最終周の continue は `exhausted`。repeat ステップの outcome は break→ok、exhausted→exhausted、cancelled→cancelled。
6. **エラー方針**: agent/check が error/timeout で終わった場合、`retries`（≤3）まで自動でリトライする。それでも駄目なら:
   - `policy.on_error = gate`（既定）: step_error ゲート（retry / 失敗扱い(fail) / 停止）を開く。
   - `fail`: その outcome のまま扱い、エッジで分岐する。
7. **並行性と予算**:
   - 書き込み agent は単独で走る。read の agent と check は、書き込み agent と同時には走らない。
   - 計算ステップの同時実行数は `max_parallel` 以下。
   - agent/check の試行回数は `max_steps` で、計算時間は `max_active_secs` で打ち切る。
   - 超過時は `on_budget = gate`（既定）で予算ゲート（延長 / 停止）を開く。**黙って失敗しない**。
8. **書き込み前認可**: `step_started` が担当・プロンプト blob・期限・コマンド・access を完全に記述する。その行を fsync してから spawn する。
9. **フィードバックの受け渡し**（cascade の「毎ホップ同じタスク」を解消する）:
   - 失敗した check の出力の末尾、verdict の指摘、人間のコメント（P4 では却下したハンクとコメント）を `FeedbackItem` にする。
   - 次の `iteration_started.feedback`（`last` = 直前の周の分、`all` = それまでの全周の分。上限 16 件）に記録し、プロンプトの `{{feedback}}` に渡す。
10. **テンプレート**: 使えるのは `{{goal}}`、`{{inputs.x}}`、`{{iteration}}`、`{{feedback}}`、`{{instructions}}`、`{{nodes.<id>.output|outcome}}` だけ。式も条件もない。`check.command` はテンプレート化しない（シェル注入を防ぐため）。
11. **停止・一時停止・回復**:
    - stop{graceful}: 実行中のステップの完了を待つ。stop{cancel}: 計算ステップを `cancel: stop` で取り消す。その後、開いているゲートを `gate_cancelled{stopped}` にし、**階層ごとに内側から**片付ける（内側のイテレーションを `cancelled` で閉じる → その repeat ステップを最後の決定に従った outcome で終える → 外側のイテレーションを閉じる…）。最後に `loop_finished{cancelled, stopped}` で終わる。既に break した repeat は ok、解決済みのゲートのステップは選択肢の outcome で終える（§8.2 の規則どおり）。
    - pause{drain}: 何も新しく始めない。pause{cancel}: 計算ステップを `cancel: pause` で取り消し、resume で `cause: resume` として再試行する。
    - recovery ゲートが開いている間は、新しい計算を始めない（ループは pause しない）。

### 4.4 検証と停止性

- `validate(doc, env) -> Validation{rev, blueprint?, diagnostics[], bounds?}` は純関数。`agentpit-events` にあり、デーモン・Tauri・CLI が共有する。
- 診断コード（snake_case、安定）:
  - 形・版: `invalid_shape`、`schema_unsupported`、`doc_too_large`、`unknown_value`、`unsupported_feature`
  - 名前・入力・予算: `invalid_name`、`invalid_input_name`、`budget_out_of_range`
  - ノード: `no_nodes`、`too_many_nodes`、`invalid_node_id`、`duplicate_node_id`、`unsupported_node_kind`、`empty_text`、`text_too_large`、`cast_conflict`、`retries_out_of_range`、`timeout_out_of_range`、`invalid_path`、`invalid_gate_options`、`max_iterations_out_of_range`
  - 構造: `unknown_parent`、`parent_not_repeat`、`parent_cycle`、`nesting_too_deep`、`empty_repeat`
  - エッジ: `unknown_edge_endpoint`、`cross_scope_edge`、`edge_on_not_allowed`、`duplicate_edge`、`cycle`（メッセージは「repeat で包め」）
  - テンプレート: `invalid_placeholder`、`unknown_input`、`unknown_template_node`
  - 警告: `unknown_role`、`unknown_backend`、`budget_below_worst_case`
- メッセージは英語の1文で、最後に直し方を書く（AI 設計器へそのまま返せるように）。
- **停止性の論拠**:
  - 各スコープは有限の DAG で、各ノードはインスタンスあたり最大 1+retries 回。repeat は ≤20 周、入れ子 ≤3。したがって `worst_steps = Σ (agent/check: 1+retries, gate: 0, repeat: max_iterations × worst(子))` は有限で、検証時に表示できる。
  - 加えて、全ステップに期限があり、ゲートは期限切れになるか、止まるだけで計算を消費しない。予算は常に効く。
  - manager 経路の唯一の硬いガードが再帰深さだけ（`max_calls_per_manager` はプロンプトの文）であるのに比べ、**厳密に強い**。
- **rev** = `b1-` + FNV-1a-64（`layout` を除いた正規化 JSON）。キーの順序に依らず、ノードをドラッグしても変わらない。

### 4.5 既存資産からの橋渡し（P3）

- `[[workflow.steps]]` → ブループリントへのインポート（`agentpit blueprint import --workflow <type>`）:
  - 線形の agent の鎖にする。`roles[0]`→role、`backends[0]`→backend、persona/behavior→task。
  - `ask:true` なら後ろに gate を挿入する。`dynamic:true` なら manager ノードにする（P3）。`fanout` は警告を出して落とす。
- Studio の localStorage のスケッチ → 一度きりのインポート**ボタン**。自動移行にすると、seed の実体化バグ（`StudioApp.jsx:194/200/474`）を引き継ぐため。戻りエッジは「repeat で包む」提案に変える。
- AI 設計: `agentpit workflow new --format blueprint --json`。designer_prompt / extract_json / normalize を再利用する。`validate` の診断を1回だけモデルに返して修復させる。AI は**提案するだけ**で、保存と開始は人間が行う。

## 5. 状態スキーマ（問い合わせ）

- **LoopState**（fold の結果。`state.rs`）:
  - ループ: `status`、`pause`、`stop`、`finish`、`budget`、`usage{steps, active_ms}`、`epoch`、`head_seq`、`last_ts`
  - 実行: `steps`（StepRun）、`instances`（InstanceRun）、`repeats`（RepeatRun）、`gates`（GateRun）、`instructions`
  - 管理: `ops`（op_id → 最初の seq）、`blocking`（解釈できない critical レコード）、`warnings`
  - 導出: `is_waiting()`、`orphaned_steps()`、`pending_instructions()`、`writable()`、`summary()`、`check_cursor()`
- **LoopSummary**（1行 = ボードの行 = P2 の head.json）:
  - 識別: loop_id / uid / title / blueprint{name, rev}
  - 状態: status と `waiting`、pause / stop / finish
  - 段階と担当: `active[]`（step_id / node / iter / kind / attempt / assignee / run_id / started_ts）、`iterations[]`（repeat / outer / n / max / open）
  - 人間待ち: `open_gates[]`（最大5件、プロンプトは 280 バイトに切り詰め）、`open_gate_count`、`pending_instructions`
  - 消費: usage / budget、head_seq / updated_ts、writable
- **問い合わせ面**:

| 問い合わせ | ソケット | 返り値 | フェーズ |
|---|---|---|---|
| `loop_list{include_terminal?, limit?}` | daemon | `LoopRow{summary, runner: live\|absent\|starting}`（head.json + registry。プローブなし） | P2 |
| `loop_attach{since?: Cursor, chunks}` | runner | `loop_attached{uid, head_seq, reset}` の後、`loop_record` をディスクから再生し、以後はライブ | P2 |
| `loop_status` | runner | LoopSummary | P2 |
| `loop_read{what: output\|prompt\|check_log, step_id, offset}` | runner | バイト範囲 | P2 |
| `watch{}` | daemon | 全ループの LoopSummary（変化したときだけ）+ `loop_gone` | P3 |
| `blueprint_list/get/validate/save{base_rev}` | daemon | 行・文書・診断・新しい rev | P3 |
| `LoopView`（Tauri emit `loops:view`） | bridge が fold から射影 | summary + 凍結ブループリント + ノード別の状態 + 直近のステップ + ゲート + イテレーション | P3 |

- 既存の workflow 経路（manager）の「どのエージェントか」は、これまでどおり events.jsonl の Tracker が答える（変更なし）。

## 6. イベントスキーマ（ジャーナル）

- **エンベロープ**: `{"v":1,"seq":N,"ts":ms,"kind":"…","op":"<op_id>"?,"anc":true?,"data":{…}}`
  - `seq` はジャーナルごとに 1 から連続。順序は seq で決まる。`ts` は表示用で、非減少にクランプする。
  - `anc` = ancillary（旧リーダ・旧ライタが無視してよい種別）。既知の種別では表が決め、行の値は使わない。
  - `loop_id` はエンベロープに入れない（ファイルで決まる。wire では包みが持つ）。
- **レコード種別（21種。C=critical、A=ancillary）**:

| 分類 | 種別 |
|---|---|
| ライフサイクル | `loop_created`(C、常に seq1。凍結ブループリント・rev・入力・cwd・workspace・予算・origin・root_run_id)、`writer_opened`(C: epoch/pid/start_id/build/schema_minor/切り詰めたバイト数)、`writer_closed`(A) |
| 制御 | `loop_started`、`loop_paused{drain\|cancel}`、`loop_resumed`、`loop_stop_requested{graceful\|cancel}`、`loop_finished{succeeded\|failed\|cancelled, reason, node?}`、`budget_changed` |
| スケジューラの判断 | `iteration_started{repeat, iter, feedback[]}`、`iteration_finished{break\|continue\|exhausted\|cancelled}`、`node_skipped{dead_path\|scope_ended\|stopped}` |
| ステップ | `step_started`（**書き込み前認可**: 担当・run_id・プロンプト blob・コマンド・期限・access・消費した指示・cause・retry_of）、`step_spawned`(A: pid/start_id。孤児の始末用)、`step_finished{outcome, exit_code?, verdict?, output?, excerpt?, log_tail?, error?, feedback[], cancel?}`、`step_interrupted{epoch}` |
| 人間 | `gate_opened{kind, step_id?, prompt, options[{id,label,outcome?}], deadline_ms?, on_timeout?}`、`gate_resolved{option, comment?, by}`、`gate_cancelled{stopped\|timed_out\|superseded}`、`instruction_received{text, target?, by}` |
| 診断 | `warning`(A) |

- **予約名**（このビルドでは Unknown として扱う）: `blueprint_revised`（P3）、`workspace_ready/failed`、`proposal_created/decided/landing/landed/closed`、`outcome_labeled`(A)、`step_tool`（以上 P4）、`artifact_created/preview`（P5）。
- **エフェメラル**（ジャーナルに書かない。seq はない。取りこぼしうる）:
  - `loop_chunk{loop_id, step_id, offset, text}`。`offset` は `outputs/<step>.log` のバイト位置で、遅れて来たクライアントはファイルから埋める。
  - `loop_lagged`、`heartbeat{head_seq}`（15 秒ごと）。
  - **規則**: 再接続後に必要なものはすべてジャーナルに書く。ライブ専用のものは「ジャーナルの行 + ファイル」から再構成できなければならない。
- **例**（抜粋。全行は `tests/fixtures/loops/journal_fix_until_green.jsonl`）:

```jsonl
{"v":1,"seq":12,"ts":1790380944990,"kind":"step_finished","data":{"step_id":"test.i1.a1","outcome":"fail","elapsed_ms":14543,"exit_code":101,"log_tail":"thread 'nested' panicked at src/parser.rs:131","feedback":[{"source":"check","node":"test","step_id":"test.i1.a1","summary":"cargo test -q exited 101","detail":"thread 'nested' panicked at src/parser.rs:131"}]}}
{"v":1,"seq":13,"ts":1790380945002,"kind":"iteration_finished","data":{"repeat":"fix","iter":[1],"decision":"continue"}}
{"v":1,"seq":23,"ts":1790381211000,"kind":"gate_resolved","op":"0199a1c0-7b1e-7f00-9c11-3e5f6a7b8c9d","data":{"gate_id":"g1","option":"approve","by":{"kind":"human","client":"agentpit-dashboard/0.3.0"}}}
```

- **wire フレーム**（P2）: `{"event":"loop_record","loop_id":"lp-…","rec":<ジャーナルの行そのもの>}`。ライターは1度だけシリアライズし、ディスクの行と wire の `rec` を同じ内容にする。中継（デーモン・ブリッジ）は `LoadedRecord.raw`（元の行）を流し、型から再エンコードしない。こうすると、新しいライターが足したフィールドや値が、古い中継を通っても消えない。
- **数値**: `agentpit-events` は serde_json の `float_roundtrip` を有効にしている。凍結したブループリントの浮動小数点（React Flow の座標や未知のキー）が、ジャーナルを往復しても同じ値に戻り、fold と rev が一致する。

## 7. 操作スキーマ

- 共通: 全操作に `op_id`（冪等キー）を付ける。`expect_seq`（任意の CAS）も付けられる。wire は平らな形: `{"loop_id","op_id","op":"resolve_gate","gate_id":"g1","option":"approve"}`（`ops.rs` の `OpRequest`）。
- 応答:
  - 成功 = `OpResult{op_id, outcome: applied|duplicate|noop, seq?, head_seq, result?}`
  - 失敗 = `OpError{code, message, details?}`。message は1文で、最後に次の一手を書く。
- 却下した操作はジャーナルに書かない。同じ接続では、応答は**それが生んだレコードの後**に届く（read-your-writes）。
- **LoopOp**（P1 で型を定義、P2 で実行する）と前提条件:

| op | 受け付ける状態 | noop | 主なエラー |
|---|---|---|---|
| `start` | created | running / paused | stopping・終端 → invalid_state |
| `pause{drain\|cancel}` | running | paused | created・stopping・終端 → invalid_state |
| `resume` | paused | running | それ以外 → invalid_state |
| `stop{graceful\|cancel, reason?}` | created / running / paused、graceful 停止中の cancel | 同じモードで停止中 | 終端 → invalid_state |
| `cancel_step{step_id}` | 実行中の agent/check ステップ | — | 不明 → not_found、実行中でない → invalid_state |
| `resolve_gate{gate_id, option, comment?}` | 開いているゲート（paused でも可） | — | 不明 → not_found、閉じている → invalid_state（details に誰がどう答えたか）、不正な選択肢 → validation |
| `instruct{text, target?}` | 非終端 | — | 空・長すぎ → validation、target が agent でない → validation |
| `set_budget{max_steps?, max_active_secs?, max_parallel?}` | 非終端、上限内、`max_steps ≥ 使用済み` | 変化なし | validation |
| 未知 | — | — | unsupported |

- **エラーコード**: `bad_request`、`unsupported`、`not_found`、`invalid_state`、`conflict`（expect_seq 不一致）、`validation`、`read_only`、`busy`、`gone`、`unavailable`、`internal`（+ Unknown）。
- P4 で `decide_proposal`、`land`、`instruct.context` を、P5 で `regenerate_preview` を追加する。
- **デーモン側の verb（P2）**:
  - `loop_start{op_id, blueprint: {source: path|inline|named|builtin}, inputs, cwd, title?, start=true, origin}` → `loop_started{loop_id, socket, duplicate}`
  - `loop_ensure{loop_id}`、`loop_list`、`loop_stop_runner{loop_id, force}`
- 例:

```
→ {"id":1,"type":"hello","proto":1,"features":["loops/1"],"client":"agentpit-dashboard/0.3.0"}
← {"id":1,"ok":true,"data":{"kind":"hello","proto":1,"role":"daemon","features":["loops/1","bp.node.agent","bp.node.check","bp.node.gate","bp.node.repeat","bp.workspace.in_place"]}}
→ {"id":7,"type":"loop_op","loop_id":"lp-0199…","op_id":"0199a1c0-…","op":"resolve_gate","gate_id":"g1","option":"approve"}
← {"event":"loop_record","loop_id":"lp-0199…","rec":{"v":1,"seq":23,…,"kind":"gate_resolved",…}}
← {"id":7,"ok":true,"data":{"kind":"op_result","op_id":"0199a1c0-…","outcome":"applied","seq":23,"head_seq":25}}
```

## 8. 状態機械（遷移表。1レコード = 1遷移。`admit` の検査を併記）

`admit` の拒否コード: `missing_header`、`duplicate_header`、`invalid_header`、`bad_epoch`、`bad_status`、`unknown_node`、`kind_mismatch`、`bad_step_id`、`bad_iter`、`instance_decided`、`bad_retry`、`budget_exhausted`、`concurrency`、`incomplete_effect`、`step_not_found`、`step_not_running`、`outcome_not_allowed`、`gate_pending`、`iteration_open`、`iteration_not_open`、`bad_iteration`、`scope_busy`、`gate_not_found`、`gate_not_open`、`bad_option`、`bad_gate_subject`、`bad_id`、`empty_text`、`too_large`、`budget_out_of_range`、`unknown_value`。全コードに固定テストがある（`admit_rejects_each_documented_violation`）。

### 8.1 ループ

| 現状態 | レコード | 次状態 | ガード |
|---|---|---|---|
| — | `loop_created` | created | 空の状態でだけ。loop_id・uid・予算・rev の一致・**作成時の validate** |
| created | `loop_started` | running | |
| running | `loop_paused` | paused | |
| paused | `loop_resumed` | running | |
| created / running / paused | `loop_stop_requested` | stopping | stopping → stopping は graceful から cancel への昇格だけ |
| running | `loop_finished{succeeded\|failed}` | succeeded / failed | 実行中のステップ 0、開いたゲート 0 |
| stopping | `loop_finished{cancelled}` | cancelled | 同上 |
| running* | （導出） | waiting* | 計算ステップ 0 かつ開いたゲートあり |
| 終端 | writer_opened / writer_closed / warning だけ | 同じ | P4 で提案系を追加 |

### 8.2 ステップ（試行。終端は変わらない。リトライは新しいステップ）

running →（step_finished）→ done(outcome) ｜ running →（step_interrupted。旧エポックの計算ステップだけ）→ interrupted。

- 開始の条件:
  - ループが running であること。ノードが存在し、kind が一致すること。
  - 文脈: iter の長さ = 囲む repeat の数で、各 repeat のその周が開いていること。
  - step_id が `step_id(node, iter, attempt)` と一致すること。
  - attempt 1 は未決のインスタンスだけ。attempt > 1 は agent/check だけで、直前の試行が interrupted か、error/timeout/cancelled で終わっていること（ゲートの上書きで done になっていないこと）。retry_of = 直前の試行。直前の試行についてゲートが開いていないこと。
  - 計算ステップ: 予算内、並行性の規則、access・期限・（agent なら）担当とプロンプト・（check なら）コマンドがそろっていること。check は read。
  - 消費する指示が存在し、未消費で、target が合っていること。指示は一度だけ消費される。リトライのプロンプトには、retry_of をたどって前の試行が消費した指示を入れ直す（プロンプト blob に残るので、クラッシュ後も失われない）。
  - agent の access はブループリントの値を下回れない（write の agent を read として記録できない。記録を誤ると他の計算と並行してしまうため）。
  - 計算時間（`usage.active_ms`）が `max_active_secs` に達していたら、新しい計算ステップは始められない。
- outcome の許可: agent/check = ok/fail/error/timeout/cancelled、gate = ok/fail/timeout/cancelled、repeat = ok/exhausted/cancelled。`cancel` は outcome が cancelled のときだけ付ける。
- gate ステップの outcome はゲートの閉じ方に一致する（選択肢の outcome、timed_out → timeout、それ以外の取消 → cancelled）。ゲートが開いている間は終われない。
- repeat ステップの outcome は最後の周の決定に一致する（break→ok、exhausted→exhausted、それ以外→cancelled）。周が開いている間は終われない。
- step_interrupted は、それより前のエポックで始まった計算ステップだけ。

### 8.3 ノードインスタンス（導出）

pending（未記録）→ running → done(実効 outcome) ｜ interrupted ｜ skipped。

- **実効 outcome**: 最新試行の outcome。ただし、解決した step_error/recovery ゲートで選んだ選択肢が outcome を持つなら、そちらで上書きする（例: 「失敗扱い」→ fail、「編集を残して続行」→ ok。インスタンスは done になる）。

### 8.4 repeat のイテレーション

| 現状態 | レコード | 次状態 | ガード |
|---|---|---|---|
| repeat ステップ実行中・未開始 | `iteration_started{n=1}` | open(1) | ループは running。n ≤ max |
| closed(n-1, continue) | `iteration_started{n}` | open(n) | ループは running。n ≤ max |
| open(n) | `iteration_finished{break\|continue\|exhausted\|cancelled}` | closed(n) | スコープ内に実行中のステップがなく、スコープ内のステップについて開いたゲートもない。continue ⇒ n<max、exhausted ⇒ n=max |
| closed（最後） | repeat の `step_finished` | — | break→ok、exhausted→exhausted、それ以外→cancelled |

### 8.5 ゲート

| 現状態 | レコード | 次状態 | ガード |
|---|---|---|---|
| — | `gate_opened` | open | ID は連番。選択肢は 1〜8 個で ID は一意。同じステップについて開いたゲートがないこと。種類ごとの主体（approval = 実行中の gate ステップで、ループは running。step_error = error/timeout で終わった agent/check の最新試行。recovery = interrupted の agent/check の最新試行。budget = 主体なしで、開いた予算ゲートがないこと）。step_error/recovery は、(1) その試行について解決済みの step_error/recovery ゲートがない（インスタンスは一度だけ決まる）、(2) 試行のスコープ（周）がまだ開いている、(3) 選択肢の outcome は ok か fail だけ、を満たすこと。approval 以外は paused 中も可 |
| open | `gate_resolved` | resolved | 選択肢 ∈ options |
| open | `gate_cancelled` | cancelled | 停止・置換・期限切れ（on_timeout がないとき） |

- 期限が来たとき、on_timeout があれば `gate_resolved{by: timeout}` にする。
- 既定の選択肢:
  - approval: approve(ok) / reject(fail)
  - step_error: retry / fail(fail) / stop
  - budget: extend / stop
  - recovery: retry / mark_done(ok) / stop

### 8.6 指示

queued（`instruction_received`）→ consumed（`step_started.instructions` に入った時点）。ループが終端になれば、未消費のまま捨てる（導出）。

### 8.7 提案（P4）

| 現状態 | レコード | 次状態 |
|---|---|---|
| — | `proposal_created`（review ゲートの開始時、または終了時に landed≠head） | open |
| open | `proposal_decided{approve\|request_changes\|reject, accepted[], rejected[], comments[]}`、受理が1件以上 | landing |
| open | decided（受理なし） | declined |
| landing | `proposal_landed{conflicts: []}` | landed |
| landing | `proposal_landed{conflicts: [...]}` | conflicted |
| open | `proposal_closed{superseded\|expired\|discarded}` | closed |

- ハンクは pending ⇄ accepted ⇄ rejected。バイナリ・リネーム・モード変更は、ファイル単位の擬似ハンクとして扱う。

### 8.8 成果物（P5）

created →（`artifact_preview{started}`）→ converting → ready(pages) ｜ failed（理由と対処）。regenerate で failed/ready → converting に戻る。新しい版が出ると、旧版は superseded（導出）になる。

## 9. 永続化

### 9.1 ファイル配置

```
$XDG_STATE_HOME/agentpit/
  loops/<loop_id>/
    journal.jsonl           唯一の真実。単一ライター（ランナー）
    head.json               LoopSummary のキャッシュ（P2。tmp+rename。ジャーナルより先行しない）
    prompts/<step_id>.md    レンダ済みプロンプト（秘密情報を伏せ、step_started より先に fsync）
    outputs/<step_id>.log   ライブ表示テキスト（chunk の耐久コピー）/ outputs/<step_id>.md 最終回答
    checks/<step_id>.log    check の全出力（ジャーナルには末尾 4KiB）
    proposals/ artifacts/ wt/   （P4/P5）
  loop-leases/<fnv(loop dir)>/owner.json   セッションのリースを再利用（キーはディレクトリ）
  daemon/loops/<loop_id>.json   ランナーの登録（P2。registry.rs と同じ形）
$XDG_RUNTIME_DIR/agentpit/loop-<32hex>.sock    42 バイト（paths.rs の 48 バイト予算内）
<repo>/.agentpit/blueprints/*.json, ~/.config/agentpit/blueprints/*.json
git: refs/agentpit/loops/<id>/{base,landed,head}   （P4。gc から保護し、push しない）
```

### 9.2 ジャーナルの不変条件（`journal.rs`）

- I1 単一ライター: リースを持つものだけが追記する。
- I2 seq は 1 から連続し、1行目は `loop_created`。
- I3 **バッチ = 1回の write_all + sync_data の後にだけ**、適用・配信・応答・エフェクトの開始をしてよい。
- I4 ライターは、最初の追記の直前に**最後の改行より後ろを切り詰める**。未終端の行は、完全な JSON に見えても採用しない。リーダはそれを無視するだけ。
- I5 途中の破損・seq の不連続は read-only にし、自動修復しない。
- 1行は 256KiB 以下。大きなペイロードは blob ファイルに置く。作成は一時ファイル + rename で原子的に行う。

### 9.3 コミット経路

`LoopJournal::commit` が唯一の経路:

1. `writable()` を確認する。
2. 状態の仮コピーに対して、各ドラフトを `admit` し、仮に `apply` する（バッチ内で前のレコードを前提にできる）。
3. 1バッチで追記して fsync する。
4. 本来の状態に `apply` する。

いずれかのドラフトが拒否されたら、何も書かない（全部か無か）。

### 9.4 クラッシュ回復（ランナーの起動時。P2）

1. リースを取る（Busy なら、生きているランナーがいるので終了する）。
2. スキャンして fold する。書けないなら read-only で配信する（ファイルには触れない）。**P2 の実装**: ランナーは書けないジャーナルを開かずに終了し、デーモンの `loop_ensure` が `read_only`（壊れた行・新しい版の記録・理解できない凍結ブループリント）を返す。クライアント（CLI、P3 のブリッジ）は同じ fold でディスクから直接表示する。
3. `writer_opened{epoch+1, truncated_tail_bytes}` を書く（このとき末尾を切り詰める）。計算時間は、旧ライターの最後のレコードまでで数える（クラッシュの空白は数えない）。
4. **孤児を始末する**: `orphaned_steps()` の各ステップについて、`step_spawned` の pid/start_id が同じ実体なら SIGTERM を送り、2 秒後に SIGKILL を送る。
   - **P2 の実装**: 始末はプロセス**グループ**単位。check は自分のグループで走り、agent はランナー自身のグループ（ランナーはグループリーダー）で走るので、agent の孤児は旧ランナーの `writer_opened.pid` のグループとして始末する。リーダーが生きていて start_id が違う（pid の再利用）なら触らない。自分のグループは決して触らない。
   - **check の起動は `step_spawned` の fsync を待つ**: check のシェルは stdin の1行を待ってから本体を実行し、ランナーは `step_spawned` を書いてからその1行を送る。ランナーがその前に死ねば stdin が閉じて本体は走らない。したがって、始末できない check の孤児は生まれない（§4.3 規則 8 の強化）。
5. マトリクスに従って処理する:

| 対象 | in_place | worktree（P4） |
|---|---|---|
| agent（旧エポックで実行中） | `step_interrupted` + recovery ゲート（**自動で再実行しない**。ゲートが開いている間、新しい計算は始めない） | チェックポイントへ戻して `step_interrupted`、同じ担当で再試行 |
| check | `step_interrupted` → 自動で再試行（cause: recovery） | 同左 |
| gate / repeat | そのまま継続（期限は再設定） | 同左 |
| 着地中の提案（P4） | — | 3方向の照合。判断できなければゲート |

### 9.5 カーソル

- クライアントが保持するのは `Cursor{uid, seq}` だけ。サーバはクライアントごとの状態を持たない。
- `check_cursor`: uid が違う → Reset（最初から再生）、seq > head → Reset、それ以外 → seq より後を再生。
- ジャーナルは存続中に圧縮しない（予算で上限があり、数千行・数 MB 程度）。したがって任意の seq から再生できる。

### 9.6 保持

- 終端したループのうち、200 本を超えた分か 30 日を過ぎたものを、デーモンのスイーパが削除する（worktree、refs、ディレクトリ）。
- 削除しないもの: 非終端のループ、未着地の提案があるループ。
- 表示するだけなら `read_loop()`（リース不要）を使う。終端したループをライターとして開き直さない（writer_opened が無駄に増えるため）。

## 10. 既存ログとの関係

- **events.jsonl（テレメトリ + ルーティングの入力）**:
  - ループの状態は一切書かない。新しい Event variant も RunKind の値も追加しない。
  - ループのルートは `RunKind::Workflow`、`role="loop:<name>"` にする。各 agent ステップはその子 run（`parent_run_id` = ルート、depth+1、role はノードの role かノード id）。**既存ダッシュボードの Workflow Run 表示が、改修なしでループを木として表示する**。
  - 相関は環境変数ではなく明示で渡す（P2: `RunLogger::start_linked`、`RunStarted.loop_ref` は任意フィールドとして加算。`dashboard/src-tauri/src/state.rs:205` の網羅的な分解に `..` を足す）。
  - ランナーは継承した env（`AGENTPIT_PARENT_RUN_ID` など）を消して起動する。ノードの期限はランナー自身が執行する。
  - 能力と関係のない失敗は `LegStatus::Skipped` にする（cascade の先例）。対象は取消・中断・auth/unavailable・ワークスペース準備の失敗。check の失敗とゲートの却下は events.jsonl に書かない。
  - **学習ラベル**: RouteDecided.task_hash はレンダ済みプロンプト全体から取る。反復ごとにフィードバックで変わるので、「6h 以内の再送は失敗」ルールで偽のラベルが付かない（Q3）。OutcomeNoted は P4 の人間の明示的な判定でだけ出し、`outcome_labeled`(A) に記録して二重発行を防ぐ。
- **セッション JSONL**: 変更しない。TUI から起動したループは `origin.session_id` を持ち、セッション側に ext `agentpit.loop_link{loop_id}` を書いてよい（再生では透過）。
- **asks/ メールボックス**: src/ask は変えない。任意の**ミラー**（P2 の任意コミット、`[loops] mirror_gates_to_asks`）は、ゲートを `asks/<id>.json` にも書き、既存の Needs-You コックピットで答えられるようにする。op の経路と先勝ちで競う。

## 11. トランスポートとプロセスモデル

- **ループはどこで走るか**: 専用のランナー `agentpit daemon loop --loop <id> --socket <path>`（隠しサブコマンド）。
  - デーモンが `process_group(0)`、null stdio、`kill_on_drop(false)` で spawn する。デーモンより長生きする。
  - デーモンの中では走らせない（状態を持たないブローカのままにする）。セッション worker の中でも走らせない（busy が1枠なので並行できない）。前景の CLI でも走らせない。
  - **1ループ = 1プロセス**: クラッシュが隔離され、単一ライターが自然に成立する。
- **ランナーの内部**: 1つの actor（tokio タスク）が `LoopJournal` と接続表を持つ。入力（Op / StepDone / Timer / Attach / Shutdown）ごとに次を行う:
  1. 純関数のスケジューラ `sched::next(&state, now)` で次の手を決める。
  2. commit（admit → 追記 → fsync → apply）する。
  3. 配信する。
  4. エフェクトを起動する。エフェクトはジャーナルに触れず、完了を報告するだけ。
- **attach**: actor の中で原子的に行う（応答が、以後のどのフレームよりも先に届く）。再生はディスクから行う。接続ごとのチャネルは有界で、満杯になったら chunk・heartbeat から捨てる。耐久レコードが積めなければ、カーソルのヒントを付けて切断する。
- **hello と機能交渉**:
  - `PROTO_VERSION` は 1 のまま（上げると、厳密一致による kill の連鎖が起きる。`server.rs:37-50`）。
  - `hello.features` を加算的に使う: `loops/1`、`blueprints/1`（P3）、`watch/1`（P3）、`proposals/1`（P4）、`artifacts/1`（P5）、`bp.node.<kind>`、`bp.workspace.<mode>`。
  - クライアントは広告されていない verb を送らない。要求ごとにタイムアウトを付ける。features のない旧 hello は、これまでどおりに動く。
- **全体 vs 個別**: 全体 = daemon の `watch`（P3。各ループの head.json を見て、LoopSummary が変わったときだけ push）。詳細 = ランナーへの attach。ループをまたぐ全順序はない（どの画面も必要としない）。
- **Tauri ブリッジ**（P3、`dashboard/src-tauri/src/bridge/`、`agentpit-events` だけに依存）:
  - デーモンを `daemon/owner.json` の socket で見つける。なければ同梱のサイドカーで `agentpit daemon start` する。
  - **同じ fold で畳む**。webview は状態機械を再実装しない。
  - emit: `loops:board`、`loops:view`、`loops:chunks`、`loops:inbox`（ゲート + 既存の AskCard）、`daemon:status`。
  - コマンド: `loops_board`、`loop_open/close`、`loop_start`、`loop_control`、`gate_resolve`、`step_output`、`blueprints_*`、`blueprint_design`（P3）、`proposal_*`、`instruct`、`editor_read/write`（P4）、`artifact_page`（P5）。

## 12. 画面 ↔ 状態 ↔ レコード ↔ 操作（UI 契約）

| 画面 | 見るもの | 読む状態 | 変化させるレコード | 送る操作 | 遅延目標（p95） |
|---|---|---|---|---|---|
| ループボード | ループ・段階（ノード＋n/max）・担当・人間待ち・経過 | LoopSummary[] | 全コア種別 | start / pause / resume / stop | 稼働中 ≤300ms |
| キャンバス（設計／実行） | 設計はブループリントの編集。実行は凍結版 + ノード状態の色・反復バッジ・担当チップ・出力の末尾 | LoopView | step_* / iteration_* / gate_* / node_skipped + loop_chunk | blueprint_save、start、cancel_step | ノード遷移 ≤150ms |
| 受信箱 | 全ループの開いたゲート + 既存の AskCard | open_gates（退避中は head.json） | gate_* | resolve_gate / answer_ask | ≤300ms。2つ目の窓には誰がどう答えたかを出す |
| 差分レビュー（P4） | 提案ごとのファイル・ハンク、受理／却下、着地、競合 | ProposalHead + 不変の patch | proposal_* | decide_proposal、land | ≤100ms |
| エディタ（P4） | ファイル、選択範囲 →「ここだけ直して」、インラインの merge ビュー | EditorContext + 提案 | instruction_received、proposal_* | instruct / builtin:edit の開始、editor_write（CAS） | ≤200ms |
| 成果物（P5） | ページ列、変換中／失敗、版の切り替え | ArtifactView | artifact_* | regenerate_preview | ready 後 ≤300ms |

## 13. 互換性と進化のルール

- **既存の弱点とこの設計での扱い**:

| 弱点（検証済み） | 扱い |
|---|---|
| `#[serde(other)]` がなく、新しい variant で行ごと消える | 二段デコード（エンベロープ → 種別）。未知の種別は保存し、カーソルは必ず進む。新しい enum はすべて Unknown で終わる |
| 既知の entry の中の未知の enum 値で、偽の recovery が起きる | 状態を駆動する未知の値を含む critical レコードは **read-only** にする（書かない） |
| 未知の verb に id 0 で応答し、ハングする | P2(a) で Value を先にパースして呼び出し元の id を返し、`unsupported` にする。クライアントにタイムアウトを付ける |
| PROTO_VERSION の厳密一致による kill の連鎖 | 上げない。features で交渉する |
| events.jsonl に seq も版もない | 遡って直さない。ループの事実は新しいジャーナルへ。events.jsonl には任意のフィールドを足すだけ |
| settings.rs と config.rs の手書き二重スキーマのドリフト | 新しい面の型は agentpit-events に1つだけ置く。ブループリントは生の JSON を正とする |

- **規則**:
  - C1: エンベロープのフィールド（v, seq, ts, kind, op, anc, data）は凍結。
  - C2: 加算的な変更では `SCHEMA_MINOR` を上げる（writer_opened に記録される）。旧版が無視してよい新種別は `anc:true`、それ以外は critical。自分より新しい minor のライターが書いたジャーナルは read-only にする（新しいフィールドを黙って落とさないため）。
  - C3: 意味・型・既定値を変えない。改名は alias で行う。削除は「書かなくなるが読み続ける」。
  - C4: **正規キャリア規則**。コアの遷移は必ずコアの種別で運ぶ。新しい種別は詳細を足すだけで、唯一の運び手にはならない。
  - C5: Unknown は描画するだけで、決して作用しない。
  - C6: クレートをまたぐ識別子（backend、role、model、run_id）は文字列。
  - C7: `v: 2` は新しいジャーナルとして始め、v1 を書き換えない。
  - C8: ブループリントの未知のノード種別は、保存はできるが実行はできない。未知のキーは生の文書に残る。意味を変えるキーには `requires` を使う。
  - C9: 状態を駆動する未知の値の判定（`has_unknown_values`）は、`apply`/`admit` が分岐に使う enum をすべて含める（step の cancel 理由とゲートの取消理由を含む）。表示専用の enum（skip/close 理由、surface、actor）は含めない。凍結ブループリントの `schema` が v1 以外なら、実行できないものとして扱う。
  - C10: 既存の enum（Event/RunKind/LegStatus/BackendId/ExchangeStatus）への catch-all の追加は、学習と可用性の意味を変えるので**別の PR** にする（リーダを先に出し、新しい値を書き始めるのは1リリース後）。
- **固定テスト**: 全種別の golden 行（`kinds_v1.jsonl`）、未来の行（未知の種別・値・v2・フィールド）、fixture ジャーナルの再生と逐次 admit、全拒否コード、全診断コード。

## 14. 原則の改訂（オーナー承認済み＝Q1、2026-09-26）

`docs/agent-hub-design.md` の原文は書き換えずに、該当箇所の直下へ日付付きの改訂ブロックを追記した。

### 14.1 「静的DAGなし」（agent-hub:145）

> **改訂**: manager 駆動の `agentpit workflow` には静的 DAG を課さない（**不変**）。ただし、人間が保存して開始したブループリントから生まれたループに限り、実行系が段の順序・有界ループ（max_iterations 必須、≤20、入れ子 ≤3）・ゲート・予算を拘束する。任意のサイクルは書けず、静的な最悪ステップ数を検証時に示す。

- **変わる点**: オプトインの第2のオーケストレーション経路ができる。
- **保たれる点**: `agentpit workflow` と MCP の `run_workflow` のプロンプトは変わらない（既存の固定テストがそのまま緑）。`[[workflow.steps]]` は manager へのヒントのまま。AI は提案するだけ。
- **安全な理由**: 停止性が manager 経路より厳密に強い。構造は目に見え、版が付き、誰が決めたか追える。

### 14.2 「ロールが固定するのは CAST であって SCRIPT ではない」（agent-hub:183）

> **改訂**: CAST は引き続きロール（`resolve_role` は不変）。ブループリントは**人間が所有する明示的な SCRIPT** だが、固定するのは**制御フロー（順序・分岐・有界反復・検査・ゲート・予算）だけ**である。エッジは閉じた outcome の語彙でしか分岐しない。ノードの内側（分解・道具の使い方）は即興のまま。即興を丸ごと入れたいときの逃げ道は `manager` ノード（P3）。

### 14.3 「常駐 Conductor クラスを作らない」「状態所有型 Conductor は manager より重い SPOF」（agent-hub:288-289, 317）

> **改訂**: **状態を所有する**常駐 Conductor は作らない（維持）。ループランナーは次の性質を持つ、状態を所有しない再起動可能な実行器である。
> (a) 唯一の状態が fold(ジャーナル) で、全作用は耐久化した認可レコードの後に行う。
> (b) 1ループ1プロセスで、障害の影響範囲は1ループに限られる。
> (c) LLM を含まない決定的なコードである。
> (d) 作用の出口は既存の choke point（dispatch_continuing、RunLogger、verify、git）だけ。
> kill -9 の後に respawn しても、再生で同じ状態に戻る。デーモンは状態を持たないブローカのまま。

- **SPOF 論の反転**: manager LLM の計画はモデルの文脈の中にしかなく、クラッシュで失われる。ランナーは、読み直せないものを一切持たない。

### 14.4 「宛先/topic + カーソル … 第2の長命 reader が出現したら」（agent-hub:322）

> **un-defer（条件成立）**: Tauri ブリッジと、ランナー自身の回復が長命の reader にあたる。カーソルは**ループジャーナルの seq に限り**、クライアントが保持する（サーバ側にクライアントごとの状態を持たない）。events.jsonl と Note は、カーソルなし・宛先なしのまま。

### 14.5 明示的に改訂しないもの

- worker はステートレスなワンショット（「ここだけ直して」も新しい dispatch）。
- src/ask は人間専用で、一般化しない。
- UDS + NDJSON、TCP と JSON-RPC は不採用（session-persistence §5.1）。
- 脅威モデル（worktree は事故防止であり、セキュリティ境界ではない）。

## 15. ACP の位置づけ

- **立場**: アプリ内のエディタ（CodeMirror 6）と core の間は NDJSON で話す。内部で JSON-RPC は使わない。「ACP 準拠」は3層で実現する。
- **(1) データ形の写像**（P4 で実装し、往復テストで固定する）:

| agentpit | ACP |
|---|---|
| `Selection{path, range{start{line, character}, end}}` | `Position/Range`（0 始まり、終端は排他、既定は UTF-16） |
| 提案のファイル | `ToolCallContent::Diff{path, oldText, newText}` |
| approval ゲート／提案の着地 | `session/request_permission`（allow_once / reject_once） |
| ノードインスタンスの状態 | `Plan.entries`（pending / in_progress / completed） |
| chunk / tool | `AgentMessageChunk` / `ToolCallUpdate` |
| ループ管理・ハンク単位の判定 | `_agentpit/loop/*`、`_agentpit/proposal/decide_hunks`（`_` 接頭の拡張メソッド = 「ループ管理は独自に拡張」） |

- **(2) 南向き**（P4、任意）: opencode の ACP クライアントで、`fs.writeTextFile` を広告して書き込みを worktree に入れる。ToolCall/Diff/Plan を捨てずに流す。キャンセル時に `session/cancel` を送る。
- **(3) 北向き**（P6、任意、Q9）: `agentpit acp serve` を ACP Agent（stdio）として提供する。デーモンのクライアントとして NDJSON に翻訳するだけ。Zed などから、選択範囲付きのプロンプトで提案を受け取れる。
- **やらないこと**: ACP をデーモンのプロトコルにしない。常駐の ACP エージェントを持たない。ACP の permission でゲートを迂回させない。

## 16. 型の置き場所とモジュール計画

- **規則**: serde 型・fold・検証は **agentpit-events** に置く（dashboard はこれだけに依存する）。プロセス・git・dispatch は本体のクレートに、ブリッジは dashboard に置く。
- **agentpit-events（P1、実装済み）**:

| ファイル | 中身 |
|---|---|
| `loops/mod.rs` | 定数・ID・パス・共有の値型（Outcome、NodeKind、Access、WorkspaceMode、Actor、Assignee、BlobRef、Verdict、FeedbackItem、Budget、Usage）、`string_enum!` |
| `loops/blueprint.rs` | Blueprint の型付きビュー、BlueprintIndex、canonical_json / blueprint_rev、placeholders、validate、Bounds |
| `loops/record.rs` | Record エンベロープ、KINDS / RESERVED_KINDS、全ペイロード、LoopEvent、decode_line / encode_record、LoadedRecord |
| `loops/state.rs` | LoopState（apply / admit / writable / summary / check_cursor）、RejectCode、LoopSummary、Cursor |
| `loops/ops.rs` | LoopOp、OpRequest、OpResult、OpOutcome、OpError、ErrorCode |
| `loops/journal.rs` | scan_bytes / scan_file / read_loop、JournalWriter、LoopJournal（create / open / commit）、Draft、WriterInfo |

- **P2 以降の追加**:
  - `agentpit-events/src/wire.rs`（`src/daemon/protocol.rs` から移し、`pub use` で互換を保つ）、`loops/claim.rs`（claim と head.json）、`RunLogger::start_linked`（P2）
  - `JournalTail`、`LoopView`（P3）
  - `loops/context.rs`、`loops/diff.rs`（P4）
- **本体クレート**: `src/loops/{mod,runner,sched,prompt,recovery,serve}.rs`、`src/loops/exec/{agent,check,gate}.rs`、`src/verify.rs`（arena と cascade の統合）、`src/daemon/{server,registry}.rs` の拡張、`src/cli/{loop_cmd,blueprint_cmd}.rs`。P4 で `src/gitutil.rs`、`src/loops/{workspace,land}.rs`。P5 で `src/loops/artifacts.rs`。
- **dashboard**: `src-tauri/src/bridge/`、`loops_cmd.rs`、`editor_fs.rs`（P4）。`frontend/src/loops/`（Board・Inbox）、`studio/`（設計／実行の切り替え）、`review/`・`editor/`（P4）、`artifacts/`（P5）。それぞれ純関数のモジュール + node:test。

## 17. 実装フェーズ（各フェーズ = テスト付き・コミット分離）

### P1: スキーマの実装（ステップ1。**完了**）

- **内容**: §16 の表の6ファイル。依存の追加なし、本体クレートと dashboard は無改修。
- **受け入れ基準と結果**:
  1. 全21種別に固定行が1本ずつあり、往復で `Value` が一致する。予約名と既知の種別は交わらない。→ `every_kind_has_exactly_one_fixture_line_that_round_trips`、`kinds_and_reserved_kinds_are_disjoint_and_unique`
  2. 未来の行（未知の種別 critical / ancillary、v2、未知の値、未知のフィールド、壊れたペイロード）が文書どおりにデコードされ、critical なものだけが書き込みを止める。→ `future_lines_decode_as_documented`
  3. fix-until-green のジャーナルを再生すると succeeded になり、全行が直前までの状態に対して admit される。→ `fix_until_green_replays_to_succeeded`、`every_fixture_record_is_admissible_in_order`
  4. 全拒否コードと全診断コードに少なくとも1件のテストがある。→ `admit_rejects_each_documented_violation`、`each_error_code_is_reachable`
  5. 途中の破損・seq の欠番は read-only で、ファイルに触れない。未終端の末尾は完全な JSON でも切り詰め、次の seq が続く。2つ目のライターは Busy。巨大な行は書く前に拒否する。作成は原子的。→ `journal.rs` のテスト群
  6. 回復: 孤児の検出 → interrupted + recovery ゲート → retry は admit され、mark_done は retry を禁じる。クラッシュの空白は計算時間に数えない。→ `recovery_fixture_…`、`crash_recovery_…`
  7. `cargo fmt --check`、`cargo clippy --workspace --exclude agentpit-dashboard --all-targets -- -D warnings`、`cargo test` が緑。

### P2: デーモンの窓口（ステップ2。1 PR、4〜5 コミット）

- **コミット**:
  - (a) **wire の移動**（挙動の変更なし + 既知のバグ修正）: `agentpit_events::wire`、`hello.features/client`、`Response.code/details`、`Event::Unknown`/`ResponseData::Unknown`、Value の事前パースによる id の返送、`Conn::request` のタイムアウト。単独の PR として先に出してもよい。
  - (b) **テレメトリの明示リンク**: `RunLogger::start_linked`、`RunStarted.loop_ref`（dashboard の state.rs:205 に `..` を追加）。
  - (c) **ランナー**: actor と、純関数で重点的にテストする `sched::next`。ノードは agent（role/backend/ルータ、プロンプトのレンダと blob 化、verdict）、check（統合 verify）、gate（期限つき）、repeat。in_place のみ。予算、回復マトリクス、孤児の kill、op の分類（§7 の表）。attach（ディスクからの再生、read-your-writes、有界チャネル）、chunk の offset、head.json、env の除去。
  - (d) **デーモン verb と CLI**: `loop_start`（ブループリントの解決・検証・凍結・claim、ループごとの ensure mutex）、`loop_ensure`、`loop_list`、`loop_stop_runner`。CLI は `agentpit loop start|ls|show|watch|op|gate|validate`。
  - (e) 任意: asks ミラー。**P2 では見送り**（Q4。P3 の受信箱で扱う）。
- **除外**: worktree と提案、manager/ensemble/arena ノード、watch、ブループリントの CRUD、待機中ループの退避、Tauri。
- **実装メモ（2026-09-26。設計から確定・変更した点）**:
  - **CLI**: `agentpit loop start|ls|show|watch|pause|resume|stop|gate|instruct|cancel-step|budget|validate`（汎用の `op` の代わりに操作ごとの動詞）。ループ id は一意な前方一致・後方一致で指定できる。`--no-start` で作ったループは `resume` で開始する。全要求にタイムアウトを付ける（attach 後のストリームを除く）。
  - **機能交渉**: デーモンは `features` を送ってきたクライアントにだけ `loops/1` などを返す（旧クライアントへの hello はバイト単位で不変）。CLI は `loops/1` がなければ古いデーモンだと告げて止まる。ランナーの hello は `role: "loop"`。
  - **終端したループ**: ランナーは終端すると `writer_closed{terminal}` を書いて終了し、ルート run を閉じる。`loop_ensure` は終端したループを `gone` で返し（ライターとして開き直さない、§9.6）、CLI の show/watch はディスクから読む。同じ op_id の `loop_start` が終端済みのループに当たれば `duplicate` で socket なし。
  - **ランナーの登録**: ランナー自身が bind 後に `daemon/loops/<id>.json` を書き、正常終了時に消す（デーモンが途中で落ちても登録が欠けない）。`loop_stop_runner{force}` はランナーのプロセスグループ（agent を含む）と、ジャーナルに記録された check のグループを kill する。
  - **`loop_start`**: 同じループ id の作成はループごとの mutex で直列化する（同時の再送は `duplicate`）。ルート run の id は先に採番して `loop_created` に入れ、`RunStarted` は作成に成功してから出す（`RunLink::run_id`）。パス指定のブループリントは通常ファイルだけを上限つきで読む。
  - **スケジューラ（レビューで確定）**:
    - 自動リトライの回数は、そのインスタンスの error/timeout で終わった試行だけで数える（一時停止・回復・指示による再実行は数えない）。
    - 中断した agent は access に関係なく recovery ゲート（read の agent もツールで副作用を持ちうる）。check だけが自動で再試行する（§9.4 の表どおり）。
    - 計算時間は実行中の区間を含めて数える（`LoopState::active_ms_at(now)`）。並列の計算が途切れなくても `max_active_secs` が効く。
    - 予算ゲートは、予算が足りるようになった（`set_budget`）か、トップレベルが終わろうとしている時点で `gate_cancelled{superseded}` にする（開いたままだと `loop_finished` が admit されない）。
    - 上限（`HARD_MAX_*`）に達して `extend` が効かないときは、ゲートを開かず `on_budget: fail` と同じく停止して `failed/budget` で終える。
    - `loop_stop_requested.reason = "budget"` はスケジューラ専用。人が同じ文字列で止めた場合は `"budget (stopped by a person)"` に書き換える（`cancelled/stopped` で終わる）。
  - **プロンプト**: 入れ子の repeat の中の agent には、外側から内側までの全イテレーションのフィードバックを渡す（内側の1周目が空でも、外側のレビュー指摘が届く）。リトライ（retry_of の連鎖）は前の試行が消費した指示を引き継ぐ（§8.2）。`prompts/<step>.md` は既存の秘密情報マスク（`exec::redact_secrets`）を通して保存し、agent には元の文面を送る。VERDICT 行は Markdown の装飾（`**VERDICT:** PASS` など）を無視して読む。
  - **配信**: 1つのバッチの途中で送れなくなった接続には、それ以降のレコードを送らない（欠番を作らない）。chunk・heartbeat は接続キューに 1/4 の余裕があるときだけ送る。`loop_read` は文字の途中で切らない。
  - **受け入れ基準の対応**: 1・2・4・5・7 は `src/loops/runner_tests.rs`（偽の exec + 実 UDS）、3 は同ファイルの in-process 版と `tests/loop_e2e.rs`（実バイナリで runner を kill -9、check の孤児が始末され自動で再試行される）、6 は `src/daemon/server.rs` と `wire.rs` のテスト、8 は CI。基準 5 の「1k レコード」は 60 件の連続 commit 中の attach で確認している。
  - **敵対的レビュー**: 4 観点（スケジューラ・ランナー・回復とデーモン・操作と CLI）で 37 件、懐疑的検証で 35 件を確認し、すべて修正した（上記の各項目と回帰テスト）。
- **受け入れ基準**（実際の UDS と偽の ExecAdapter で。worker.rs の EchoExec/SlowExec の手法）:
  1. plan → repeat{implement → check（1回目は失敗、2回目で成功）} → gate が、2周目で break → waiting → CLI で承認 → succeeded になる。2周目のプロンプトに check の末尾がフィードバックとして入っている。
  2. events.jsonl に Workflow のルート（`role:"loop:fix-until-green"`）と、`parent_run_id` = ルートの子 run がある。
  3. agent の実行中にランナーを kill -9 → ensure で `writer_opened{epoch 2}`・`step_interrupted`・recovery ゲート → `retry` で完走する。自動の再実行はなく、孤児のプロセスも残らない。
  4. 同じ op_id → duplicate（同じ seq）。解決済みのゲートへの別の op → invalid_state。`expect_seq` の不一致 → conflict。
  5. `attach{since:k}` が (k, head] をちょうど1回ずつ届ける。1k レコードを連続で commit しながら同時に attach しても、欠落も重複もない。
  6. 旧 hello（features なし）のセッションの動作が変わらない。未知の verb には同じ id で `unsupported` を返す。
  7. `max_steps` を使い切ると予算ゲートが開き、extend で続く。
  8. 既存の daemon/TUI のテスト、clippy、dashboard のビルドが緑。

### P3: 画面で状況確認とループ実行（ステップ3。2〜3 PR）

- **内容**:
  - Tauri ブリッジ（owner.json による発見、同じ fold、emit とコマンド）。
  - ループボード、受信箱（ゲート + AskCard）。
  - Studio を「設計／実行」の切り替えにする。実行時は凍結したブループリントに状態を重ね、repeat は React Flow のグループノード、n/max バッジ、担当チップ、出力の末尾を出す。
  - ブループリントファイルの CRUD（`base_rev` による楽観的並行制御、未知のキーの保存）と、localStorage からのインポートボタン。
  - `workflow new --format blueprint`、daemon の `watch`、waiting/paused のランナーの退避と起床、`manager` ノード。
- **受け入れ基準**:
  - キャンバスから開始し、ノード遷移の表示が p95 ≤150ms。
  - 稼働中にアプリ・デーモン・ランナーをランダムに kill（20 回）しても、最終表示が `agentpit loop show --json` と一致する。
  - 退避中のループのゲートが受信箱に出て、答えると 2 秒以内にランナーが起きる。
  - 古い base_rev での保存は conflict のダイアログになる。未知のキーが往復で残る。
  - 旧 Workflow Run の表示と asks は変わらない。
- **実装メモ（2026-09-26。設計から確定・変更した点）**:
  - **射影**: `LoopView`（`agentpit-events/src/loops/view.rs`）は summary・凍結ブループリント・ノード別の状態（表示するインスタンスは囲む repeat の現在の周。開いたゲートがあれば `waiting`）・直近 50 ステップ・ゲート・指示・警告・`source`（scope・path・cwd・inputs）を持つ。ブリッジも CLI も同じ fold から作る。
  - **ブループリントの CRUD はファイル**（`agentpit-events/src/loops/store.rs`）: `<project>/.agentpit/blueprints/` と `~/.config/agentpit/blueprints/`。保存は `base_rev`（= `blueprint_rev`）の CAS と tmp+rename、新規は既存があれば conflict、文書の name はファイル名と一致させる。未知のキーは文書ごとそのまま保存する。全クライアントが同じマシンにいるので、§5 の表の daemon verb（`blueprint_*`）は作らず、ブリッジと CLI が同じ store を直接使う。
  - **watch**: daemon の `loop_watch{include_terminal}` は `loops`（その時点の行）を返し、以後 200ms ごとに各ループの head.json とジャーナルの stat を見て、変わった行だけ `loop_row`、消えたループを `loop_gone` で流す。CLI は `agentpit loop ls --watch [--json]`。
  - **head.json は信用しすぎない**: head.json の `head_seq` がジャーナルの最後の確定行の seq（末尾数 KB だけ読む `journal::last_seq`）と一致するときだけ使い、ずれていればジャーナルを fold する（ランナーが追記と head.json 更新の間で死ぬと、終端したループの行が永久に古いままになるため）。
  - **退避と起床**: ランナーは、計算ステップも接続もなく、停止中でも終端でもない状態（waiting・paused・未開始）が `[loops].park_after_minutes`（既定 10 分、0 で無効。テスト用に `AGENTPIT_LOOP_PARK_SECS`）続くと `writer_closed{idle}` を書いて終了する。起こすのは (1) 操作（どのクライアントでも `loop_ensure` を経由する）と (2) デーモンの waker（15 秒ごと。開いたゲートの最早の期限 `LoopSummary.next_deadline_ms` の 20 秒前）。`loop show` とブリッジの表示は起こさない（ディスクから読む）。
  - **manager ノード**: `NodeSpec::Manager`（task・backend=claude|codex・model・effort・workflow（型）・access・verdict・retries・timeout、既定 2 時間）。ジャーナル上は **agent ステップ**として記録する（新しいレコード種別も enum 値も足さない。旧版は凍結文書の `manager` を未対応として read-only にする）。ランナーは `cli::workflow::manager_cast`（`run_capture` と同じ解決順）で manager を先に決めて `step_started` の担当（role `manager`、route `manager`）と先行採番の run id に書き、`run_capture` を `ManagerLink` 付きで走らせる（ループのルート run の子になる）。出力は `outputs/<step>.log` に流れ、合成結果が answer になる。
  - **AI 設計**: `agentpit workflow new --format blueprint "<説明>" [--json] [--write]`。実在のロール名と backend id をプロンプトに固定し、検証エラーがあれば診断を1回だけ返して修復させる（修復が壊れていれば最初の案と診断を返す）。`--json` は `{doc, rev, runnable, diagnostics, repaired}`、`--write` はプロジェクトの `.agentpit/blueprints/<name>.json` に新規作成だけ。ダッシュボードの「✨ 生成」は提案を新しい名前で保存して設計キャンバスで開く（開始は人が行う）。
  - **ブリッジ**（`dashboard/src-tauri/src/bridge/`）: 発見は owner.json → runtime dir の socket、応答がなければ同梱 CLI の `agentpit daemon start`（10 秒に1回まで）。`loops/1` のないデーモンは `daemon_outdated` で止める。
    - ボード: `loop_watch` 1本を張り続け（再接続は 250ms→5s）、行の塊は 15ms の静穏か 60ms で1回にまとめて `loops:board` と `loops:inbox` を出す。行からゲートが消えたらジャーナルを読んで「誰がどう答えたか」を受信箱の履歴に足す（2つ目の窓の要件）。
    - ビュー: `loop_open` はまずディスクから fold して即座に返し、ランナーが live（またはあるべき＝running で人待ちでない。落ちたランナーの回復）なら cursor 付きで attach する。退避中・終端・read-only はジャーナルのサイズを 500ms ごとに見るだけで、起こさない。接続が切れるたびにディスクを読み直す（ディスクが正）。直後に `writer_closed` があるループは、行が live と言っていても attach しない（退避直後の古い行で起こさないため）。`loops:view` は 40ms に1回まで（静かな後の最初の変化は即時）、`loops:chunks` は 50ms ごと。
    - コマンド: `loops_board` `daemon_status` `loop_open/close` `loop_start`（origin=dashboard、op_id で冪等）`loop_control` `gate_resolve` `step_output`（ディスクから。そのループのステップだけ）`blueprints_list/get/save/delete/validate` `blueprint_generate`。エラーは `{code, message, details}`。
  - **画面**（`dashboard/frontend/src/loops/`。Workflow Run・Learning と同じ島の形）: ボード（状態・段階・担当・予算・一時停止/再開/停止）、受信箱（全ループのゲート + 既存の asks。退避中は「答えると起きる」と表示。最近の回答と回答者）、ブループリント（一覧・新規・削除・開始・✨ 生成・Studio スケッチの取り込み）、キャンバス（設計／実行の切り替え）。
    - 実行: 凍結ブループリントにノードの phase/outcome、repeat のグループと n/max、担当チップ、発火したエッジ、選択（または自動追従）したステップの出力末尾（ディスクの末尾 + ライブ chunk。欠けたらディスクから読み直す）、ゲートの回答、指示、ステップの取り消し。
    - 設計: ノードの追加・接続・移動・削除、種別ごとのインスペクタ、文書の設定、生 JSON、検証（300ms の debounce、ノードの強調）、`base_rev` 付きの保存と conflict ダイアログ（編集を続ける／自分の編集を捨てて読み直す／上書き）、保存してから開始。編集は未知のキーを保つ純関数（`canvas.js`）で行う。
    - Studio の localStorage スケッチの取り込みは一度きりのボタン（描いたものだけ。seed は出さない）。線形の鎖にし、ask→gate、dynamic→manager、fanout とサブスウォームは注記して落とす（§4.5）。
  - **受け入れ基準の対応**:
    - ノード遷移: ヘッドレスの Chromium で偽のブリッジから 40 回の遷移を流し、イベントから DOM の反映まで p95 17ms（ブリッジ側の間引きは最大 40ms）。
    - ランダム kill: `tests/loop_e2e.rs` の `random_kills_of_daemon_runner_and_display_end_on_what_loop_show_says`（実バイナリで runner・daemon・表示を 20 回 kill -9。表示の最後の行が `loop show --json` と一致し、ジャーナルの seq に欠番がない）と、ブリッジの `random_kills_of_runner_daemon_and_app_end_on_the_disk_fold`（runner 接続・daemon 接続・アプリ全体を 20 回。最後の `loops:view` がディスクの fold と一致）。
    - 退避中のゲート: `a_parked_loop_wakes_when_its_gate_is_answered_and_watchers_see_it`（実バイナリで 5 秒以内に完走。起床自体は ensure の直後）と、ブリッジの `looking_at_a_parked_loop_does_not_wake_it`。
    - conflict と未知のキー: store のテストとブリッジの `unknown_keys_round_trip_through_save_and_get`、キャンバスの `canvas.test.js`。
    - 旧 Workflow Run と asks: 既存のコードと命令は変更なし（受信箱は `get_pending_asks`/`answer_ask` をそのまま使う）。

### P4: 編集機能（ステップ4）

- **内容**:
  - worktree モード: 一時の `GIT_INDEX_FILE` で**作業ツリーのスナップショットコミット**を作る（未コミット・未追跡も含む。index には触れない）。refs は base/landed/head。書き込みステップごとにチェックポイントを作る。
  - 提案（landed..head、内容アドレスのハンク、`previously_rejected`/`out_of_scope`）、review ゲート、`decide_proposal`/`land`。
  - ファイル単位の `git merge-file` による着地（index を変えない、原子的な rename、競合時は同意なしにマーカーを書かない）。着地のクラッシュ対策（landing/landed）と照合。
  - OutcomeNoted と `outcome_labeled`。
  - `EditorContext` と `instruct.context`、`builtin:edit`（repeat{agent → review}）。
  - CodeMirror 6 + `@codemirror/merge`、`editor_read/write`（CAS）。
  - ACP の形の写像と、南向きの強化（任意）。
- **受け入れ基準**:
  - 3 ハンク中 2 を受理 → ディスク = base + その 2 ハンクちょうど。`git diff --cached` は空。
  - ユーザの重ならない編集はきれいに併合され、重なる編集は競合として報告され、ファイルは変わらない。
  - 着地中の kill → 回復で同じ結果になる。却下したハンクとコメントが次の周のプロンプトに入る。
  - worktree モードでは、エージェントがユーザの作業ツリーを変えない（カナリアで確認）。

### P5: pptx のプレビューと作成（ステップ5）

- **内容**:
  - 成果物の検出（宣言、または提案内のメディア種別）と、内容アドレスでの保存。
  - `soffice --headless -env:UserInstallation=<ループ専用> --convert-to pdf` → `pdftoppm -png`。上限 120 秒・50 ページ、直列。
  - `artifact_*` のレコードと regenerate の操作、ページビューア。
  - python-pptx で作るテンプレートのブループリント（agent → check（pptx が開けるか）→ review）。
- **受け入れ基準**:
  - 10 枚のデッキが 30 秒以内にページ表示される。
  - soffice がなければ `preview_failed` として「`pacman -S libreoffice-fresh poppler`（日本語は `noto-fonts-cjk`）」を案内する。
  - 着地したバイナリがバイト単位で同一。

### P6（任意）: `agentpit acp serve`

## 18. 正直な限界とリスク

- **リースの回収競合（既存）**: `SessionLease` は、死んだ持ち主のリースを2つのプロセスが同時に回収すると、両方が取れてしまう窓がある（remove_dir_all → create_dir の間）。ループでは P2 のデーモンがループごとの ensure mutex でランナーの起動を直列化して防ぐ。リース自体の修正（rename による回収）は別の PR にする。
- **ページキャッシュの可視性**: I3 は、ライターが**配信した**ものについて成り立つ。ファイルを直接 tail するリーダは、sync_data が返る前の行を見うる。tailer は、ライターが告知した seq だけを信じる（P3 の JournalTail の約束）。
- **停電時の複数行バッチ**: 複数ページにまたがるバッチは、停電で一部の完全な行だけが残りうる。各レコードは単独で admit 可能な遷移なので、fold は正しく、ランナーの回復（§9.4）が続きを判断する。
- **計算時間の数え方**: ノート PC のサスペンド中も、実行中のステップがあれば計算時間に数える（壁時計）。`max_active_secs` の既定を長め（4h）にしている理由の一つ。
- **worktree は砂箱ではない**: フル自律のエージェントは、絶対パスで外に書ける。書き込みステップの前後でユーザの作業ツリーの `git status` の指紋を取り、差があれば `warning{workspace_escape}` とフィードバックを出す。脅威モデルは従来どおり（事故防止）。
- **学習の汚染**: Skipped の規則、プロンプト全体の task_hash、人間の明示的な判定だけでのラベル付けで抑える。learn.rs のフィクスチャテストで固定する。
- **原則の改訂が、汎用 DAG エンジンへの免罪符と読まれる**: 非目標（式言語・任意のサイクル・実行中のグラフ変更・manager の置き換え）を改訂文に明記する。
- **二つの承認系（asks とゲート）**: 受信箱を1つにし、出所のバッジを付ける。ask は manager 専用のまま。
- **テンプレートの `{{`**: v1 では `{{` は必ずプレースホルダの開始として解釈し、エスケープはない。`format!("{{}}")` のような文字どおりの `{{` を含むタスクは `invalid_placeholder` になる。回避策は、コード片をファイルに置いて参照させること。エスケープ記法は、必要になった時点で `requires` 付きの機能として足す。
- **LibreOffice の脆さ**: 専用プロファイル、タイムアウト、直列化で対処する。失敗は正常な状態であり、ループを落とさない。

## 19. 未決事項（レビューで決めたい）

- **Q1（原則の改訂）**: §14 の 3 件（静的DAG／CAST／Conductor）を、オーナーの決定として日付付きで `agent-hub-design.md` に追記してよいか。**決定（2026-09-26）: 承認**。追記済み。
- **Q2（ブループリントの形式と置き場）**: JSON。プロジェクト `.agentpit/blueprints/` とユーザ `~/.config/agentpit/blueprints/` の両方、名前が衝突したらプロジェクトを優先。TOML は将来の import/export。**提案: この形**。
- **Q3（学習ラベル）**: task_hash はレンダ済みプロンプト全体から取り（反復で変わる）、OutcomeNoted は人間の明示的な判定のときだけ出す。**提案: この保守的な形**。
- **Q4（ゲートの asks ミラー）**: 既存のコックピットからも答えられるよう、既定で on にするか。**提案: on**。P2 では未実装（P3 の受信箱と合わせて決める）。
- **Q5（in_place の中断時の方針）**: recovery ゲートを出し、自動では再実行しない。**提案: この形**。P2 で実装（agent は access に関係なくゲート、check は自動で再試行）。
- **Q6（既定のワークスペース）**: キーを省略したときの既定は、互換のため in_place で固定する。P4 以降、AI 設計器と `builtin:edit` は worktree を明示する。スナップショットには未コミット・未追跡を含める。**提案: はい**。
- **Q7（着地の意味）**: 作業ツリーだけを書き換え、index とコミットには触れない（コミットはユーザが行う）。**提案: はい**。stage/commit モードは将来。
- **Q8（待機中ランナーの常駐）**: P2 では非終端のランナーを常駐させる。P3 で waiting/paused を退避し、スイーパが期限で起こす。**提案: この段階導入**。
- **Q9（外部エディタ）**: Zed や Neovim 向けの `agentpit acp serve` は要るか。**提案: アプリ内のエディタを優先し、P6 は保留**。形の準拠は P4 で行う。
- **Q10（上限と既定）**: repeat ≤20・入れ子 ≤3・ノード ≤64・max_steps の既定 40（上限 500）・max_parallel の既定 1（上限 4）・計算時間の既定 4h・ゲートは既定で無期限。**提案: この値**。
- **Q11（保持）**: 終端したループは 200 本・30 日。worktree は終端後 14 日で削除し、refs と patch は残す。**提案: この値**。
- **Q12（エディタの部品）**: CodeMirror 6 + `@codemirror/merge`（Monaco より軽く、React island に埋め込みやすい）。**提案: CodeMirror 6**。
- **Q13（Studio の既存スケッチ）**: 自動で移行するか、インポートボタンにするか。**提案: ボタン**（seed の実体化バグを避ける）。
