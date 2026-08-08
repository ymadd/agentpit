# agentpit — セッション永続層 設計（JSONL 履歴 + デーモン + 状態機械 + CLI UX）

> Prime Agent (Prime Intellect) の永続層設計から「append-only JSONL 履歴」「デーモン +
> attach/detach」「Running/Idle/Inactive 状態機械」の3点と、CLI UX の良い部分を取り込む。
> RLM / 自己書き換え / プロセス内コード実行は**非目標**（取り込まない）。
> 追記（§10）: RLM の利点だけを隔離・可観測性・復旧可能性つきで取り込む
> **Orchestration REPL（workflow v2）**を拡張章として設計した。prime 形の RLM
> （サンドボックスなしのプロセス内 IPython・自コンテキストの直接操作）が非目標である
> ことは変わらない。
> 追記（§11）: Q5 を反転し、**TUI フロントエンド（ratatui）をスコープ内**とした
> （2026-08-08 決定）。agentpit を「スキル経由の道具」から人間の主役フロントエンドへ。
> ステータス: **全フェーズ実装完了（2026-08-08）**。P1（ログ核4コミット）→ P2（デーモン
> +worker+attach/detach+クラッシュ回復）→ P3（idle eviction+3状態roster）→ P4/A群（guidance/
> Ctrl+C二段階/スピナー/誘導）→ B群（B1〜B7、doctor 含む）→ R1〜R3（Deno サイドカーの
> orchestration REPL、`enable_repl` で workflow 統合）→ T1〜T3（ratatui インライン TUI +
> Agents/Tree オーバーレイ、素の `agentpit` は TTY で TUI 既定）。
> 実装中に設計へ入った実測修正: UDS の SUN_LEN 制限（長い XDG_RUNTIME_DIR は /tmp へ
> フォールバック）、ゾンビ pid の生存誤判定（reaper + ps stat 検出）、REPL セルは eval でなく
> モジュール import（TS 型注釈が実行時に通るため）、stale daemon owner の live-probe 乗っ取り。
> 根拠: agentpit 現状調査 + prime-agent 実装調査（2026-08-08、prime-agent は
> TypeScript monorepo `packages/coding-agent` を直接読解）。

## 0. 現状調査の要約（設計の前提）

| 観点 | agentpit の現状 | 設計への含意 |
|------|----------------|--------------|
| セッション概念 | **存在しない**。REPL の `SessionState` は完全インメモリ（`src/cli/repl/state.rs`）。永続化は rustyline の入力行履歴のみ | 移行対象データなし。ゼロから設計できる |
| JSONL 資産 | `agentpit-events` が `$XDG_STATE_HOME/agentpit/events.jsonl`（run 単位テレメトリ、追記専用 + compaction + pruning）を実装済み | スキーマ・慣用句（1行1JSON、ベストエフォート、state dir 分離）を踏襲。**events.jsonl はそのまま並存**（用途が違う） |
| マルチターン | REPL の各ターンは**独立 dispatch**（`repl/dispatch_turn.rs:190`）。会話は継続しない | バックエンド側セッションの継続（`claude --resume` 等）を新設する必要がある |
| バックエンド session ID | `StreamDecoder`（`src/exec/stream.rs`）は answer/display の分離のみ。session ID は**未抽出** | アダプタ層に抽出と翻訳を追加する（§4.3） |
| プロセス管理 | 全て `tokio::process` + `CancellationToken` 階層（`src/exec/base.rs`、`src/dispatch.rs`）。tokio は `features=["full"]` | デーモン/worker は既存の async 基盤にそのまま乗る |
| 長時間実行 | デーモン・detach・resume は皆無。ターミナルを閉じると子プロセスは SIGHUP で道連れ | 引き継ぎの3対象がそのまま新規機能になる |
| クロスプロセス IPC | ask メールボックス（ファイル + atomic rename + 250ms ポーリング、`src/ask/core.rs`）のみ。ソケットは 0 件 | UDS は新規インフラ（§5 で選定理由） |
| Windows | CI/リリースマトリクスに **Windows なし**（macOS arm64/x64 + Linux x64 のみ）。`pid_alive` は非 Unix で常に true という既知ギャップ | **UDS 一本で設計**（named pipe 抽象は入れない）。→ 質問 Q1 |
| UI | `console` + `cliclack` のみ。スピナー/TUI なし | prime の UX は console ベースで段階輸入（§7） |

prime-agent 側の実測（引き継ぎ記述との差分）:
- idle タイムアウトの実装既定値は **90分**（`DEFAULT_IDLE_EVICTION_MINUTES = 90`）。
  引き継ぎの「30分」はブログ由来の値で、実装と異なる。スイープは5分毎。→ 質問 Q4
- 「1 worker = 1 セッション」ではなく **1 worker = 1 セッションツリー**（RLM 子が同居）。
  agentpit に RLM 子はないので **1 worker = 1 セッション**に単純化する。
- leaf pointer は**ディスクに保存されない**（ロード時は「ファイル最終エントリ = leaf」）。
- 同時書き込み防止はファイル flock ではなく**リースディレクトリ**（mkdir + atomic rename +
  pid + プロセス開始時刻による PID 再利用ガード）。
- 不完全な最終行は**修復せず読み飛ばす**（torn line 許容）。
- detach は「操作」ではなく**プロセス終了の副作用**。agent loop はイベント sink
  コールバック1個だけを知っており、クライアント/ソケットの概念を持たない。

---

## 1. 全体像

```
┌─ ターミナル A ─┐   ┌─ ターミナル B ─┐   ┌─ dashboard(将来) ─┐
│ agentpit repl  │   │ agentpit attach│   │                    │
└───────┬────────┘   └───────┬────────┘   └────────┬───────────┘
        │  UDS: $XDG_RUNTIME_DIR/agentpit/daemon.sock          │
        ▼                    ▼                     ▼
┌─────────────────────── agentpit daemon ──────────────────────┐
│ セッション台帳 / roster(状態機械) / worker の spawn・監視・再接続 │
└───────┬──────────────────────────────┬───────────────────────┘
        │ UDS: workers/<sid>.sock      │
        ▼                              ▼
┌─ worker (sid=A) ─────────┐  ┌─ worker (sid=B) ─────────┐
│ セッションリース保持       │  │                          │
│ JSONL 追記(唯一の書き手)   │  │  …                       │
│ 子プロセス(claude 等)所有  │  │                          │
└──────────┬───────────────┘  └──────────────────────────┘
           ▼
$XDG_STATE_HOME/agentpit/sessions/<uuid7>.jsonl   ← 1 セッション = 1 ファイル(分岐込み)
$XDG_STATE_HOME/agentpit/session-leases/<sha256>/owner.json
$XDG_STATE_HOME/agentpit/daemon/workers/<sid>.json ← worker 台帳(再接続用)
```

- **worker がセッションファイルの唯一の書き手**（リースで強制）。デーモンは台帳と配線のみ。
- detach してもエージェントループ（= worker 内の dispatch）は影響を受けない。
  ターミナルを閉じても worker は走り続け、後から attach で戻れる。
- Phase 1 はデーモンなしでも成立する（REPL がリースを取って直接書く）。§8 参照。

## 2. JSONL スキーマ（エージェント非依存）

### 2.1 設計原則

1. **共通部分だけを構造化**する: どのバックエンドでも「何を送り」「何が返り」「どう終わったか」
   は同型。エージェント固有のストリーム（各 CLI の JSONL ログ等）は**セッションファイルに
   入れず**、既存の `runs/<run_id>/<backend>.log` への参照（`raw_ref`）で持つ。
   バックエンドが増えてもスキーマ変更ゼロ（`backend` は自由文字列、固有物は参照の先）。
2. **exchange(要求) と result(結果) を別エントリに分ける**: dispatch 開始時に `exchange` を
   追記し、完了時に `result` を追記する。**JSONL 自身がリカバリジャーナルになり**、
   result のない exchange = 「実行中に落ちた」を別ジャーナルなしで検出できる
   （prime は CommandRecoveryJournal / WorkerRecoveryJournal を別持ちしているが、
   agentpit はこの2エントリ分割で代替する — 検討過程は §6.3）。
3. **拡張は `ext` エントリ**: 未知の `type` とすべての `ext` はリプレイ時に素通しされる
   （前方互換）。将来の機能追加でスキーマ版数を上げずに済む脱出口。

### 2.2 エンベロープとエントリ型

全エントリ共通: `{"type": …, "id": …, "parent_id": …, "ts": …}` +型別フィールド。
- `id`: 8 hex のランダム ID（in-memory で衝突チェック、prime と同方式）
- `parent_id`: 追記時点の leaf の id（ヘッダのみ無し）。同じ parent_id を複数エントリが
  持てる = ファイル内で木になる
- `ts`: RFC3339

| type | 役割 | 主フィールド |
|------|------|--------------|
| `session` | ヘッダ（先頭行固定） | `v`(=1), `session_id`(UUIDv7), `cwd`, `title?`, `parent_session?`(fork 元ファイル) |
| `user` | ユーザー入力 | `text` |
| `route` | ルーティング判断 | `tool`, `category?`, `confidence?`, `backend`, `reason` |
| `exchange` | バックエンドへの要求（dispatch 開始時に追記） | `backend`, `transport`, `run_id`, `model?`, `effort?`, `prompt`, `continue_from?` |
| `result` | 要求の結果（完了時に追記、parent = exchange） | `status`(ok/error/timeout/cancelled/auth), `answer`, `exit_code?`, `duration_ms`, `backend_session_ref?`, `raw_ref?` |
| `switch` | アクティブバックエンド切替 | `from`, `to` |
| `summary` | compaction | `text`, `first_kept_id`, `reason`(manual/auto) |
| `state` | セッション状態 | `status`(active/archived/crash) |
| `label` | ノードへの命名 | `text`, `target_id?`(省略時 = parent) |
| `ext` | 拡張（リプレイ非参加） | `ext_type`, `data`(任意 JSON) |

### 2.3 具体例（1セッションの抜粋）

```jsonl
{"type":"session","id":"a1b2c3d4","ts":"2026-08-08T09:12:01.100Z","v":1,"session_id":"0198f3f2-7c1a-7000-8000-3f2a9b1c4d5e","cwd":"/Users/yamato/Work/foo","title":null,"parent_session":null}
{"type":"user","id":"b2c3d4e5","parent_id":"a1b2c3d4","ts":"2026-08-08T09:12:05.000Z","text":"このリポジトリのテスト構成を説明して"}
{"type":"route","id":"c3d4e5f6","parent_id":"b2c3d4e5","ts":"2026-08-08T09:12:05.120Z","tool":"rescue","category":"explain","confidence":0.82,"backend":"codex","reason":"profile"}
{"type":"exchange","id":"d4e5f6a7","parent_id":"c3d4e5f6","ts":"2026-08-08T09:12:05.200Z","backend":"codex","transport":"exec","run_id":"83251-kq3-1","model":"gpt-5.4-codex","effort":"medium","prompt":"このリポジトリのテスト構成を説明して","continue_from":null}
{"type":"result","id":"e5f6a7b8","parent_id":"d4e5f6a7","ts":"2026-08-08T09:12:53.400Z","status":"ok","answer":"テストは3層構成です…","exit_code":0,"duration_ms":48211,"backend_session_ref":"thread_0d9f2c","raw_ref":"runs/83251-kq3-1/codex.log"}
{"type":"user","id":"f6a7b8c9","parent_id":"e5f6a7b8","ts":"2026-08-08T09:14:00.000Z","text":"それを README に反映して"}
{"type":"exchange","id":"a7b8c9d0","parent_id":"f6a7b8c9","ts":"2026-08-08T09:14:00.150Z","backend":"codex","transport":"exec","run_id":"83251-kq3-2","model":"gpt-5.4-codex","effort":"medium","prompt":"それを README に反映して","continue_from":"thread_0d9f2c"}
{"type":"result","id":"b8c9d0e1","parent_id":"a7b8c9d0","ts":"2026-08-08T09:15:10.000Z","status":"ok","answer":"README.md を更新しました…","exit_code":0,"duration_ms":69850,"backend_session_ref":"thread_0d9f2c","raw_ref":"runs/83251-kq3-2/codex.log"}
{"type":"summary","id":"c9d0e1f2","parent_id":"b8c9d0e1","ts":"2026-08-08T10:02:00.000Z","text":"## Goal\nテスト構成の理解と README 反映…","first_kept_id":"f6a7b8c9","reason":"manual"}
{"type":"ext","id":"d0e1f2a3","parent_id":"c9d0e1f2","ts":"2026-08-08T10:03:00.000Z","ext_type":"agentpit.recovery","data":{"exchange_id":"…","note":"worker crash during exchange; outcome uncertain"}}
```

分岐の例（`f6a7b8c9` の user を撤回して別案を試す）: leaf を `e5f6a7b8` へ移してから追記。

```jsonl
{"type":"user","id":"e1f2a3b4","parent_id":"e5f6a7b8","ts":"2026-08-08T10:10:00.000Z","text":"やっぱり CONTRIBUTING.md に書いて"}
```

→ `e5f6a7b8` を親に持つエントリが2つになり、**同一ファイル内に枝が生えた**。コピーなし。

### 2.4 既存テレメトリとの関係

- `events.jsonl` / `runs/` / `tasks/` / `asks/` は**現状のまま**。セッション層は
  `run_id` を exchange に持つことで相関できる（learning/profile 系は今後も events を読む）。
- `raw_ref` の指す `runs/` は既存ポリシーで直近50 run に prune される。**raw は
  ベストエフォートのデバッグ材料**であり、answer/status はセッション側に残るので
  参照切れを許容する（prune をセッション参照で抑止する複雑化はしない）。

## 3. リーフポインタと分岐（/tree・/fork・/clone）

### 3.1 仕組み（prime 方式を踏襲）

- `leaf_id` は**メモリ上にのみ**存在する。ロード時に全行を走査し、**最後の有効エントリを
  leaf とする**。追記のたびに leaf は新エントリへ進む。
- `branch(target_id)`: `leaf_id = target_id` に**代入するだけ**。次の追記が `target_id` を
  parent に持ち、ファイル内に新しい枝ができる。ファイルコピー・書き換えは一切ない。
- 全履歴は常に残る: どの枝も `parent_id` チェーンを root まで辿れば復元できる（`/tree` は
  byId マップから木全体を再構築して描画する）。
- 既知の制約（prime と同じ、許容する）: branch 直後に**何も追記せず**終了すると、
  次回ロード時の leaf は「ファイル最終エントリ」に戻る。分岐位置の永続化が必要なら
  branch 時に `label` を1行追記する運用で足りる（自動追記はしない）。

### 3.2 三系統の分岐操作（UX は §7）

| 操作 | 出力 | 意味 |
|------|------|------|
| `/tree`(で選択) | 同一ファイル | leaf を選択ノードへ移動。user ノード選択時は本文を入力欄へ復元し編集再送できる |
| `/fork` | **新ファイル** | 選択 user ノードまでの経路を新規セッションとして切り出す（`parent_session` に元パスを記録、`git` 系の一時状態は写さない） |
| `/clone` | **新ファイル** | 現在の leaf までの経路を即複製（選択 UI なし） |

fork/clone は「別の実験を並走させたい」ときの操作なのでファイルコピーが正しい
（同一ファイル内の枝は worker=1・リース=1 で同時実行できないため）。

### 3.3 コンテキスト再構築（リプレイ）

`load()` → 1行ずつ parse（失敗行スキップ）→ byId 構築 → leaf 確定。
`context(leaf)` は leaf から parent チェーンを root へ辿って反転し:
1. 最新の `summary` があれば、その `text` を先頭に置き `first_kept_id` 以降だけを残す
2. `label`/`state`/`ext`/`route` は透過（表示用であってコンテキスト非参加）
3. 産物は `[(役割, テキスト)]` の列。用途は (a) attach 時のトランスクリプト表示、
   (b) ネイティブ継続を持たないバックエンドへの文脈合成（§4.3）

## 4. 異種バックエンドの適応（agentpit 固有の核心）

### 4.1 何が prime と違うか

prime のサブエージェントは全て prime-agent 自身なので、JSONL に LLM API メッセージを
そのまま書ける。agentpit のルーティング先は**外部 CLI で異種**。よって:
- セッションファイルには**共通形だけ**を書く（§2 の exchange/result）
- 「会話の継続」はバックエンドごとに実現手段が違うため、**アダプタが翻訳する**

### 4.2 継続トークン `backend_session_ref`

- `result.backend_session_ref` は**不透明文字列**。中身の解釈はアダプタだけが知る
  （claude: stream-json の session_id / codex: thread id / 他: 未対応なら null）。
- `StreamDecoder` に抽出フックを追加し、`DispatchResult` に `backend_session_ref:
  Option<String>` を足す（実装時に各 CLI の実出力で照合する。**ここは実挙動確認が必須**）。

### 4.3 継続の2モード

次の exchange を送るとき、アダプタの申告（`supports_resume()`）で分岐:

| モード | 条件 | prompt の中身 | CLI フラグ |
|--------|------|---------------|-----------|
| **native** | `backend_session_ref` があり、アダプタが resume 対応 | 新しい user テキストだけ | アダプタが `--resume <ref>` 等へ翻訳 |
| **compose** | ref なし / 未対応 / resume 失敗 | `summary + 直近K往復(既定4) + 新テキスト` を合成 | なし（通常の新規起動） |

- compose の合成対象を**直近 K 往復に限定**することで、ファイルサイズの O(n²) 肥大を防ぐ。
- native で resume が失敗（exit 非0 + ref 不明エラー）したら compose に**自動フォールバック**
  し、`ext {ext_type:"agentpit.resume_fallback"}` を1行残す。
- バックエンド切替（`switch`）時は ref が使えないので必ず compose になる。
  **これが「ルーターのセッション」の意味**: バックエンドをまたいで会話が続く。

## 5. デーモン + attach/detach

### 5.1 IPC の選択: Unix domain socket + NDJSON

| 候補 | 評価 |
|------|------|
| **UDS + NDJSON（採用）** | attach 中のイベントプッシュ（ストリーミング）にはコネクション指向が必要。`tokio::net::UnixListener` は標準装備。1行1JSON は events.jsonl と同じ慣用句で `socat` でデバッグ可能。prime も同構成（JSON-RPC ではなく独自エンベロープ） |
| ファイルメールボックス + notify（ask 方式の延長） | 既存慣用句だが、双方向ストリーム・多重クライアント・逆方向プッシュをファイルでやると tail/ロック/GC が自前になり、ソケットより複雑化する。ワンショット往復（ask）に限れば良い方式で、置き換えはしない |
| TCP localhost | ポート衝突・他ユーザーからの到達性・ファイアウォールの面倒だけ増えて利点なし |
| gRPC / tarpc / JSON-RPC 2.0 | フレーミングと id 相関だけが要件なので過剰。依存も重い |

- ソケットパス: `$XDG_RUNTIME_DIR/agentpit/daemon.sock`、無ければ
  `/tmp/agentpit-<uid>/daemon.sock`（ディレクトリ 0700）。**OS ユーザーごとに1デーモン**
  （全プロジェクト共通、prime と同じ。cwd はセッション側の属性）。
- プロトコル: `{"id":"…","type":"…", …}` 要求 / `{"id":"…","ok":true,"data":…}` 応答 /
  `{"type":"event", …}` プッシュ。先頭ハンドシェイク `hello {proto:1}` で版数不一致を
  即検出。serde の tagged enum で型定義（`src/daemon/protocol.rs`）。
- クライアント送信値は信用しない（環境変数等はデーモン側で許可リスト再フィルタ。
  同一 uid 前提でも prime の「peer は untrusted」姿勢を踏襲）。

### 5.2 プロセス構成: daemon / worker 分離

**1 セッション = 1 worker プロセス**（`agentpit --worker <session_id> --socket <path>`、
非公開フラグ。detached + プロセスグループ分離で起動）。

worker が持つもの: セッションリース、JSONL 追記（唯一の書き手）、バックエンド子プロセス、
per-exchange の `CancellationToken`。
daemon が持つもの: セッション台帳、roster（状態機械）、worker の spawn/監視/再接続、
クライアント⇔worker の配線。

worker を分離する理由（in-daemon tokio task 案との比較）:
1. **クラッシュ隔離**: tokio task の panic は捕捉できるが、ネイティブ層の segfault は
   プロセスごと落とす。agentpit は ONNX 埋め込み（`src/similarity/embed.rs`）という
   実在のネイティブ依存を持つ。
2. **デーモンの再起動・自己更新と実行の分離**: agentpit は self-update（`src/update.rs`）を
   持つ。worker が独立プロセスなら、デーモン更新中も実行中タスクが生き残る
   （prime: supervisor の SIGTERM は worker を道連れにしない。明示 shutdown のみ全停止）。
3. 引き継ぎ要件「セッションツリーごとに回復可能な worker プロセス」に一致。

### 5.3 attach / detach

- **attach**: client → daemon `{attach, session_id}` → daemon が worker を確保
  （Inactive なら spawn → リプレイ）→ worker がスナップショット（末尾 `transcript_tail`
  件、既定400。**モデル文脈は常に完全**で、間引くのは端末描画だけ — prime の分離を踏襲）
  → 以降 `event`（chunk / exchange 開始・終了 / 状態変化）をプッシュ。
- **detach**: 「操作」ではなく**切断の副作用**（REPL 終了・ターミナルクローズ・Ctrl+D）。
  daemon 側は client セットから外すだけで、worker のループには一切触れない。
  dispatch 済みの exchange は走り続け、result は JSONL に追記される。
- 同一セッションへの多重 attach は許可（全 client に同じイベントをファンアウト）。
  入力の同時送信は worker が直列化（実行中は後着をキューせず即エラー返し。
  キューイングは将来課題）。

### 5.4 クラッシュ回復

- **worker 監視**: ハートビートではなく**ソケット close/error**（prime 方式）。
  worker 台帳 `daemon/workers/<sid>.json` に `{pid, process_start_id, socket_path}` を記録。
  `process_start_id`（Linux: `/proc/<pid>/stat` 22 番目 / macOS: `ps -p <pid> -o lstart=`）で
  PID 再利用を排除する — 既知の `pid_alive` ギャップ（非 Unix で常に true）もこの導入で
  Unix 側は解消。
- **worker 死亡**: [250ms, 1s, 5s] で再接続試行 → プロセス生存かつ start_id 一致なら
  再接続のみ、真に死んでいれば同一 session_id で再 spawn → JSONL リプレイ →
  **result のない exchange** を検出したら `ext {agentpit.recovery}` を追記し、attach 中の
  クライアントに警告表示（外部副作用は再現できない可能性がある、の明示）。
  子プロセス（claude 等）は worker の死で孤児化するが、追跡はしない（次の exchange は
  新規/compose 継続で仕切り直し。孤児の kill は doctor の仕事、§7 B6）。
- **daemon 死亡**: worker は detached なので生存し続ける。デーモン再起動時に
  `daemon/workers/*.json` を走査 → 生存 worker へ再接続、死んだ worker は台帳から掃除。
  daemon 自身の多重起動は `daemon/owner.json`（pid + start_id、atomic rename）で排他。

### 5.5 デーモンのライフサイクル

- 明示: `agentpit daemon start|stop|status`。
- 暗黙: セッション系コマンド（repl / sessions / attach）実行時にソケットへ
  `hello` プローブ → 応答なし/版数不一致なら detached spawn（`[daemon] autostart = true`、
  false で明示起動のみ）。プローブは pidfile ではなく**ライブ接続**（prime 方式 —
  stale pidfile 問題を最初から持たない）。
- `daemon stop` は既定で worker を止めない（`--all` で全停止）。

## 6. 書き込みの堅牢性

### 6.1 不完全な最終行（torn line）

- 追記は「1行を単一 `write` + flush」。行内改行なし・4KB 未満が大半なので実質原子的だが、
  クラッシュ直撃で途切れた行は起こりうる。
- **ロード時に parse 失敗行はスキップ**し、stderr に件数警告（prime は無言スキップ。
  agentpit は observability を1段上げるが、修復・リライトはしない — append-only を守る）。
- 追記専用 + 単一ライターの前提では、破損は実質末尾にしか起きない。dangling な
  `parent_id`（スキップされた行を親に持つ）は「その枝の根」を最後の有効祖先に付け替えて
  読む（インデックス構築時の補正のみ。ファイルは触らない）。
- fsync ポリシー: 通常追記は flush まで（OS ページキャッシュに委ねる）。`result` と
  `summary` の後だけ fsync（節目の耐久性と性能の折衷）。

### 6.2 同時書き込みの防止（セッションリース）

- `session-leases/<sha256(session_file)>/owner.json` を **mkdir(原子的) + tmp 書き込み +
  atomic rename** で獲得。中身は `{pid, process_start_id, hostname, taken_at}`。
- 所有者が死んでいる（pid 消滅 or start_id 不一致）リースは reclaim。生きた所有者が
  いれば `SessionBusy` エラーをユーザーまで返す（「attach する？」の誘導つき、§7 A1）。
- prime と違い**常時有効**にする（prime は daemon worker のみ既定有効）。理由:
  Phase 1 のデーモンレス REPL 書き込みと Phase 2 の worker 書き込みが共存する期間が
  あるため、書き手が誰であってもリースを通す一本のルールの方が安全で単純。

### 6.3 リカバリジャーナルを別途持たない判断

prime は supervisor 境界の `CommandRecoveryJournal`（受領→結果→ACK の3段、再送の
二重実行防止）と worker 単位の `WorkerRecoveryJournal`（busy レコード）を別ファイルで持つ。
agentpit は §2.1-(2) の **exchange/result 分割で worker ジャーナルを JSONL に内包**した。
コマンド二重実行防止ジャーナルは Phase 2 では持たない: 副作用コマンド（send 等）は
「実行中なら即エラー」の直列化（§5.3)で衝突自体を拒むため、再送で二重 dispatch になる
窓が狭い。マルチクライアントのキューイングを入れる時に再検討する（将来課題として明記）。

## 7. 状態機械と CLI UX

### 7.1 Running / Idle / Inactive

```
                        アドレス(attach / send / sessions で選択)
        ┌──────────────────────────────────────────────┐
        │                                              │
        ▼            exchange 開始                      │ worker spawn +
   ┌─ Idle ─────────┐ ───────────► ┌─ Running ────────┐│ JSONL リプレイ
   │ worker 常駐     │              │ worker 常駐       ││ (コールドリビルド)
   │ 実行なし        │ ◄─────────── │ exchange 実行中   ││
   └───┬────────────┘  result 追記  └───────┬──────────┘│
       │ idle_evict_minutes 経過            │ worker crash
       │ (5分毎スイープ、接続0のとき)         ▼            │
       │               ┌─ Recovering ──────────────────┐│
       ▼               │ 再接続[250ms,1s,5s] → 再spawn  ││
   ┌─ Inactive ─────┐  │ → リプレイ → recovery ext 追記 ─┼┘
   │ worker なし     │◄─┴（復元不能なら Inactive + 警告）─┘
   │ JSONL のみ      │
   └────────────────┘
```

- 判定は prime の合成を単純化: `Inactive` = worker なし / `Running` = in-flight exchange
  あり / `Idle` = それ以外。
- **eviction 条件**: attach クライアント 0 ∧ in-flight exchange なし ∧
  `now - last_activity ≥ idle_evict_minutes`。eviction = worker のグレースフル終了
  （リース解放 → exit。JSONL はそのまま）。**Running は絶対に降ろさない**。
- **復帰（rehydration）**: アドレスされた瞬間に daemon が worker を spawn し、JSONL から
  フルリプレイ（キャッシュなしのコールドリビルド、prime と同じ）。agentpit のリプレイは
  テキスト走査だけなので軽く、eviction を短めに振れる。
- 設定（`config.toml`、既存の作法 = serde default + `DEFAULT_CONFIG_TOML` のコメント例 +
  往復テストに従う）:

```toml
# [session]
# idle_evict_minutes = 30   # 0 で降ろさない。既定 30（→ Q4: prime 実装は 90）
# transcript_tail = 400     # attach 時に描画する末尾件数（文脈は常に完全）
# compose_window = 4        # compose 継続時に含める直近往復数
# [daemon]
# autostart = true
# socket_path = ""          # 既定: $XDG_RUNTIME_DIR/agentpit/daemon.sock
```

### 7.2 UX 輸入計画（prime の「いい部分」）

**A 群 — セッション層に依存しない（先行投入可、各1コミット規模）**

| # | 内容 | 出典（prime での実装） |
|---|------|------------------------|
| A1 | **エラー文言規約「必ず次の一手で終わる」**: 文言を `src/cli/guidance.rs` に一元化。例: `SessionBusy` → 「別プロセスが保持しています。`agentpit attach <id>` で接続するか、`agentpit doctor` で確認してください」 | `auth-guidance.ts`（全メッセージが具体的コマンドで終わる） |
| A2 | **Ctrl+C 二段階**: REPL で実行中は1回目 = そのターンだけ中断（現行動作を維持）+「もう一度で終了」を 2 秒表示、2回目 = 終了。誤爆による取りこぼし防止 | `interactive-mode.ts`（2000ms 猶予） |
| A3 | **working 表示の格上げ**: 現行「working… Xs」を braille スピナー `⠋⠙⠹…`(80ms) + 経過秒 + 直近アクティビティ（StreamDecoder の display 行 = `[tool] name`）合成に | `loader.ts` + ローダー文言合成 |
| A4 | **行き先を教える誘導**: 廃止・改名コマンドやよくあるタイポに「unknown」でなく具体的な代替を返す（clap の suggestion に上乗せ） | `REMOVED_COMMAND_NAMES` |

**B 群 — セッション層と同時に入れる**

| # | 内容 | 対応 Phase |
|---|------|-----------|
| B1 | `agentpit sessions [--json]`: 3状態一覧の**非対話版**（プレーン表 + `--json`、スクリプト/ダッシュボード連携用）。対話一覧は T2 の Agents View（§11.3）が担う | P3 |
| B2 | attach 時「Showing latest 400 of N messages」表示 + 全文は `sessions export <id>` | P2 |
| B3 | ~~console 版 `/tree`~~ → **T2 の Tree View（§11.3）に置き換え**（console 版は作らない）。データ層（ツリー構築・branch API）のみ P1 で実装 | P1(データ層) |
| B4 | `/fork` `/clone`（§3.2 の意味論） | P1 |
| B5 | **枝を離れるときだけ要約を3択確認**（No summary / Summarize / custom）。`/compact` は確認なし一撃 — prime の使い分けをそのまま | P1 |
| B6 | `agentpit doctor [--fix]`: ソケット・worker 台帳・リース・孤児プロセスの実走査。`--fix` は**安全側のみ**（孤児ソケット/リース掃除、idle worker の停止まで。Running には触れない、強制 kill しない） | P2 |
| B7 | detach を「終了の副作用」に: REPL 終了 = 自動 detach、走行中 exchange があれば「[detached] `agentpit attach <id>` で戻れます」を最後に表示 | P2 |

**取り込まないもの（UX 面の非目標、理由つき）**

- **未知コマンドをプロンプトとして通す哲学**: prime では快適だが、agentpit の dispatch は
  課金される LLM 呼び出し。タイポ→意図しない課金実行はルーターでは事故。A4 の誘導で代替。
- **自前差分レンダラの移植（フル TUI 化）**: prime の TUI は数千行規模の独自エンジン。
  agentpit の対話面は REPL + cliclack で成立しており、費用対効果が合わない。
  B1/B3 は console の逐次描画で実現する（ratatui 導入は将来の独立判断 → Q5）。
- 安いモデルでの roster リキャップ生成（B7 相当の贅沢版）・テーマ/適応コントラスト・
  ヒントローテータ・OSC 9;4: いずれも土台が入った後に小さく足せるもので、今回は見送り。

## 8. 実装フェーズ（各フェーズ = テスト付き・コミット分離）

| Phase | 内容 | 主な新規/変更 | テスト |
|-------|------|---------------|--------|
| **P1: セッションログ核** | スキーマ・追記・リプレイ・branch/fork/clone・リース・torn line。REPL がターンを自動記録、`agentpit sessions list/show/export`、`repl --resume <id>`（デーモンなし: 直接リース + 直書き）。`backend_session_ref` 抽出（claude/codex）+ 継続2モード | `agentpit-events` に `session` モジュール（スキーマ+読み書き。dashboard が将来読めるよう共有 crate 側）、`src/session/`（leaf/branch/compose）、`src/exec/stream.rs`（ref 抽出）、`src/dispatch.rs`（DispatchResult 拡張） | 往復・分岐トポロジ・torn line・リース競合・summary リプレイ・compose 合成の各ユニット + 実 CLI での ref 抽出確認 |
| **P2: デーモン + attach/detach** | daemon/worker/プロトコル/attach/detach/クラッシュ回復/doctor。`daemon start|stop|status`、`attach`、B2/B6/B7 | `src/daemon/`（server/protocol/registry/supervisor）、`--worker` モード | プロトコル往復・detach 中の実行継続（fake backend 統合テスト）・worker kill → recovery ext 検証・daemon 再起動 → 再接続・リース越境拒否 |
| **P3: 状態機械** | idle eviction + rehydration + roster。B1 一覧 | daemon 内 sweeper、config 新キー | mock clock で eviction 発火/Running 除外・アドレス時 rehydration・設定往復（DEFAULT_CONFIG_TOML テスト） |
| **P4: UX 仕上げ** | A1〜A4（先行可。A 群は P1 と並行に別コミットで入れてよい） | `src/cli/guidance.rs` ほか | 文言スナップショット・二段階 Ctrl+C の手動確認手順 |

依存: P1 → P2 → P3。A 群はいつでも。**P1 単体でも「REPL の会話が残る・resume できる・
分岐できる」価値が出る**ため、P1 出荷 → フィードバック → P2 の順を推奨。

拡張フェーズ R1〜R3（Orchestration REPL、§10.9）は P1・P2 に依存する。位置づけは Q6。
TUI フェーズ T1〜T3（§11.4）は T1 = P2 後、T2 = P3 後。R 系と T 系は互いに独立で並行可。

## 9. 移行

- **既存セッションデータは存在しない**（調査で確認: REPL はインメモリ、events.jsonl は
  run テレメトリで用途が別）→ **移行作業なし**。
- `events.jsonl` / `runs/` / `asks/` / `repl_history` は無変更で並存。
- セッションヘッダに `v:1` を持たせ、将来のスキーマ移行は prime 同様「ロード時
  migrate、書き込みは常に最新版」方式とする。

## 10. 拡張: Orchestration REPL（workflow v2）

> レビュー会話（2026-08-08）で追加設計。永続層3対象とは独立の拡張で、実装は P1〜P2 の後。
> 引き継ぎの非目標「RLM」は prime 形（サンドボックスなしのプロセス内 IPython・
> 自コンテキストの直接操作）への判断であり**維持する**。本章はその利点 —
> 中間産物を変数に保持してコンテキストを消費しない・コードによる合成 — だけを、
> 隔離・可観測性・復旧可能性を満たす形で取り込む**別物**である。

### 10.1 動機

現行 workflow の弱点: manager が Bash/MCP tool で agentpit を呼ぶため、**全ディスパッチ
結果が毎回 manager のコンテキストに戻る**。レビュー結果 N 件のような大きな中間産物が
コンテキストを圧迫し、長いオーケストレーションほど manager が劣化する。

コード実行型ツール呼び出しの一般的な弱点（検討済み）は「事前検証の欠如」ではなく
（実行時例外のフィードバックで実用上補われ、成功率はむしろ上がる報告が主流 —
CodeAct 等）、**(a) サンドボックスなしの任意コード実行、(b) 可観測性、(c) kernel 状態の
復旧不能性**の3点。本設計はこの3点を各個撃破する構成を採る。

### 10.2 構成

```
┌─ manager（相乗り: 既存 workflow manager 経路。claude -p / codex 等）──┐
│ ツールは MCP 経由の「repl」1個だけ。ループ所有は manager 側          │
└──────────────┬───────────────────────────────────────────────────────┘
               │ コードセル（TypeScript）
┌─ worker プロセス（P2 のセッション worker）───────────────────────────┐
│  └─ deno 子プロセス（常駐、worker と同寿命 — 変数はここで生存）        │
│       deno run --allow-read=<cwd>,<artifacts>                         │
│                --allow-write=<artifacts>                              │
│                （--allow-net / --allow-run / --allow-env は与えない）  │
│                --v8-flags=--max-old-space-size=<max_heap_mb>          │
│                bootstrap.ts   ← agentpit バイナリに include_str! 同梱 │
│       stdin/stdout の NDJSON RPC（worker⇔deno、§5.1 と同じ慣用句）    │
└──────────────┬───────────────────────────────────────────────────────┘
               │ dispatch()/store/ask_human = bootstrap 内の RPC スタブ
┌─ agentpit worker の通常経路 ─────────────────────────────────────────┐
│ dispatch / ensemble / arena / ask_human（テレメトリ・権限・JSONL 完備）│
└──────────────────────────────────────────────────────────────────────┘
```

- `<artifacts>` = `$XDG_STATE_HOME/agentpit/sessions-artifacts/<session_id>/`
  （`store/` と `attach/` を含む。cwd への書き込みは deno からは不可）
- **`--allow-run` を与えないのが要**: deno から直接コマンド実行はできず、世界への作用は
  必ず `dispatch()` を通る。choke point（権限・テレメトリ・ask_human・worktree）が
  唯一の出口として維持される。
- bootstrap.ts はセルを AsyncFunction として逐次 eval し、グローバルスコープを維持する。

### 10.3 言語・ランタイム選定: TypeScript on Deno

| 候補 | 判定 | 理由 |
|------|------|------|
| Python + IPython（prime 方式） | ✗ | 外部ランタイム依存（venv/バージョン）、OS サンドボックスで「削る」方式、prime が Python なのは RLM の思想（コンテキストのデータ処理）由来で本用途（配線）には不要 |
| JS 埋め込み（rquickjs / Boa / deno_core） | ✗（当初案から変更） | capability モデルとシングルバイナリ維持は優れるが、Deno 案と二重実装になる。一本化するなら機能が枯れている Deno 側 |
| Bun サイドカー | ✗ | 埋め込み API がない（サイドカー化必至）+ サンドボックス機構なし + **TS を型チェックしない**（型を剥がすだけ）の3点で全要件と逆向き |
| **Deno サイドカー（採用）** | ✓ | permission モデル内蔵（デフォルト deny）、`deno check` 型チェック内蔵、単一バイナリ配布、async/await + `Promise.all` の合成が LLM に自然 |

- シングルバイナリ論の再評価: agentpit は**そもそも外部 CLI（claude/codex/…）がなければ
  何もできないルーター**であり、deno は「もう一つのバックエンド的依存」として既存の
  検出・availability・guidance 機構にそのまま乗る。
- deno 未インストール時は**この機能だけ無効** + A1 方式の案内（`brew install deno` 等）。
  埋め込みエンジンのフォールバックは作らない（二重実装回避）。最低バージョンは
  bootstrap のハンドシェイクで検査。

### 10.4 choke point と隔離レベル

バックエンドの隔離は非対称である（実測）: codex は `--sandbox workspace-write` で
OS サンドボックス内（`src/exec/codex.rs:24`）、claude はサンドボックスなし +
`--permission-mode acceptEdits`（`src/exec/claude.rs:19`）でユーザーのローカル設定依存。
REPL からの dispatch はこの非対称を吸収する:

1. **隔離レベルを dispatch の一級引数に昇格**（既存 `AutonomyLevel` を拡張）。REPL からの
   既定は最も保守的なレベル（codex: read-only / claude: acceptEdits を渡さない）とし、
   書き込みが必要なときだけセル側で明示指定（→ Q7）。
2. **worktree オプション**: `dispatch(task, { isolated: true })` で arena の worktree 隔離を
   流用し、本体の作業ツリーに触れさせず diff だけ返す。
3. 残余リスクの明示: サンドボックスが防ぐのは deno 内コードの直接破壊のみ。dispatch 先の
   エージェントによる間接作用は残るが、唯一の効果チャネルに権限ゲート・テレメトリ・
   ask_human を集中させる設計であり、無制限アクセスとは質が異なる。V8 層の escape は
   業界標準の隔離層（Deno Deploy 等が本番依存）に委ねる。

### 10.5 型検証（実行前チェック）

- `agentpit.d.ts`（ホスト API の型定義）をバイナリに同梱し、manager のプロンプトに提示
  （型チェックなしでも API 誤用を大きく減らすドキュメントとして機能）。
- セル実行前に `deno check` を実行し、**型エラーのセルは実行せずエラーを manager に返す**。
  実行時エラーで1往復するより速く安い。
- REPL 特有の実装点: セル間で変数が引き継がれるため、セル N のチェックは過去セルを連結した
  仮想モジュールとして行う（REPL 型チェックの定石）。連結は compaction と同期して切る。
- `[repl] typecheck = true`（既定 on、遅い環境向けに off 可）。

### 10.6 Context as variable（三層の状態モデル）

核心は**セル結果のエコーバック制御**: manager のコンテキストに返すのは切り詰めた repr
（先頭断片 + **総サイズ** — 「まだ見ていない部分がある」の認知に必須）だけで、値の全量は
deno ヒープに生きる。

```typescript
const r = await dispatch("codex", "全ファイルをレビューして");
// echo 例: r = { answer: [string 48,211 chars] "テストは3層…", status: "ok", run_id: "…" }
const s = await dispatch("claude", `重大度順に整理:\n${r.answer}`);
// r.answer(48KB)は manager のコンテキストを一度も経由しない
```

| 層 | 寿命 | 用途 |
|----|------|------|
| deno ヒープの変数 | worker/deno と同寿命（揮発） | 作業中の中間産物 |
| `store`（`sessions-artifacts/<sid>/store/`） | disk 永続。クラッシュ・eviction 後も生存 | セッションをまたぐもの。「大事なものは store へ」を .d.ts と system prompt で規約化 |
| セッション JSONL（§2） | 追記専用の履歴 | 読み取り専用の過去。下記 session API |

- **session API**: `session.answers(n)` / `session.find(q)` / `session.entry(id)` で
  過去履歴を変数に引ける。永続層の導入により「agentpit はコンテキストを所有しない」
  という RLM 非目標の根拠が**部分的に解けた**ことの帰結。ただし所有するのは共通形
  （exchange/result の answer 等）のみで、バックエンド内部の完全な会話は `raw_ref` の先の
  不透明ブロブのまま — 異種非依存の境界は維持。
- **attach 方式**: 大きな変数はプロンプト埋め込みでなく
  `dispatch(task, { attach: { name: value } })` → `attach/` にファイル化してパスを
  プロンプトに自動追記。各 CLI はファイルを読めるので、**異種バックエンドへの共通の
  大容量受け渡し形式**になる。

### 10.7 可観測性と復旧

- **可観測性**: ホスト関数境界（= dispatch の通常経路）で exchange/result・テレメトリが
  全量取れる。不透明なのはセル内の計算過程だけで、学習ルーティングへの入力は損なわれない。
- **セルログ**: 各セルをセッション JSONL に `ext {ext_type:"agentpit.repl_cell",
  data:{code, ok, echo, duration_ms}}` として追記。何を実行しどこまで進んだかは
  クラッシュ後も正確に分かる。
- **prime の kernel snapshot 方式は採らない**（JSONL とのズレ窓が原理的に残るため）。
  deno 死 → worker が検知して再起動 → ヒープ変数の消失を manager とユーザーに明示通知。
  store とセルログは生存。非決定的なセルの自動再実行はしない（危険）。

### 10.8 manager 側の限界（正直な明記）

manager のコンテキスト管理（auto-compact 等）は相乗り CLI の機能に依存し、agentpit から
制御できない。repr 切り詰めで溢れにくくした上で、超長丁場は manager 呼び出し自体を
フェーズ分割し `store` 経由で引き継ぐ（workflow の複数ステップ化と合流）。ループを
自前所有する prime に対して構造的に一段譲る部分であり、相乗り（API キー不要）の対価。

### 10.9 実装段階と設定

| 段階 | 内容 | 依存 |
|------|------|------|
| R1 | deno 検出プローブ + bootstrap + セル実行 RPC + `dispatch`/`store`/`preview` + repr 制御 + セルログ | P1（JSONL/ext/artifacts）+ P2（worker） |
| R2 | `deno check` 統合 + session API + attach + 隔離レベル引数 + worktree オプション | R1 |
| R3 | workflow type への統合（manager が repl ツールを持つ workflow 型）+ ベンチ/プロファイル対象化 | R2 |

```toml
# [repl]
# typecheck = true
# max_heap_mb = 512
# deno_path = ""        # 既定: PATH から検出
```

### 10.10 関連部品: 内部 LLM コールの相乗り trait（REPL とは独立）

diagnose の LLM 補助層・arena judge・compose 要約（§4.3）・リキャップ生成を
`LlmCompletion` trait に統一する。実装 A（既定）= 既存 dispatch の薄いラッパ
（`claude -p --output-format json` 等の単発呼び出し、サブスクリプション相乗り）。
実装 B（opt-in）= API 直（`ANTHROPIC_API_KEY`、速いが従量課金）。**REPL に依存せず
先行実装できる**小さな基盤部品で、P1〜P3 のどこに挟んでもよい。

## 11. 拡張: TUI フロントエンド

> レビュー会話（2026-08-08）で Q5 を反転して追加（ユーザー決定）。現状の
> 「Claude Code 等からスキル経由で agentpit を使う」構図に対し、**agentpit 自身を
> 人間の主役フロントエンド**にする。スキル/MCP 経由の呼び出しは従来どおり残り、
> agentpit は「他エージェントの道具」と「人間の入口」を両立する。

### 11.1 位置づけ: attach プロトコルのもう一つのクライアント

```
人間 ──> agentpit TUI ──(P2 attach プロトコル)──> daemon ──> worker ──> claude/codex/…
              │
              ├─ Conversation View（メイン、インライン）
              ├─ Agents View（← キー、フルスクリーン）
              └─ Tree View（/tree、フルスクリーン）
```

- TUI・rustyline REPL・非対話 CLI（`--json`）は**同じ attach プロトコル
  （UDS + NDJSON、snapshot + event push、§5.3）を話す3つのクライアント**。
  サーバ側（daemon/worker）に TUI 固有の変更はほぼない（Agents View 用の roster
  購読のみ P3 の状態機械と接続）。
- detach 継続・復帰・分岐という「prime 的な体験」の中身は P1〜P3 が供給し、TUI は
  その view。**デーモンなしの TUI は作らない**（体験の核が成立しないため）。
  順序は P2 後で確定。

### 11.2 技術選定: ratatui + crossterm

- Q5 当初判断の根拠「prime の自前差分レンダラを Rust で再実装する工数」は、
  ratatui への委譲で消える。
- **改訂（2026-08-08、ユーザー決定）: 全状況でフルスクリーン**。当初はインライン
  （スクロールバック保持）を既定にしたが、実装後のレビューで常時フルスクリーンへ変更。
  prime の fullscreen レイアウトを採用: エディタ + ステータスを最下部にドックし、
  トランスクリプトが**内部スクロール**（PageUp/PageDown、End で最新追従）。折返しは
  unicode-width による CJK 幅正確な自前実装（スクロール計算と描画行の一致が必須のため
  ウィジェットの word-wrap には委ねない）。オーバーレイ（Agents/Tree/Help）は同一
  alternate screen 内の描画（Mode enum）となり、入れ子の画面切替が消えた。
  トレードオフとして端末スクロールバックには履歴が残らない — 永続履歴はセッション
  JSONL と `sessions show/export` が担う。
- テーマは `src/tui/theme.rs` に一元化（prime の dark テーマ実測値をパレットに、
  パレット→セマンティック→コンポーネントの2層構造。userMsgBg カード・working pulse
  ◇◈◆◈・Hint ローテータ・一回きりヘッダーを prime から、角丸入力ボックスを
  opencode から輸入）。
- `console`/`cliclack` はワンショット経路（rescue 等）にそのまま残す。

### 11.3 画面構成

- **Conversation View**: トランスクリプト（スクロールバック、再 attach 時は
  `transcript_tail` 件 + 「Showing latest N of M」）＋ マルチライン・エディタ
  （Shift+Enter 改行）＋ ステータス行（backend / route 理由 / スピナー + 経過 +
  直近アクティビティ = StreamDecoder の display 行）。Ctrl+C は二段階（A2 と同仕様）。
- **Agents View**（← キー）: Running `◇◈◆◈`（アニメ）/ Idle `●` / Inactive `✓` の
  3 セクション。選択で attach 切替。検索（曖昧一致）は T2 後半。
- **Tree View**（`/tree`）: 罫線描画（`├─ └─ │`）、**カーソル `›` と現在位置 `•` の
  分離表示**、user ノード選択で本文をエディタへ復元して編集再送（§3.2）。
  `/fork` `/clone` もここから。
- **キーバインドは単一ソース**: バインド表を一元定義し `?` ヘルプを自動生成
  （リバインドしてもヘルプが古びない — prime UIUX 調査の Top10 項目）。

### 11.4 実装段階

| 段階 | 内容 | 依存 |
|------|------|------|
| T1 | `agentpit tui`: Conversation View（attach・入力・ストリーミング・スピナー・Ctrl+C 二段階・終了 = detach） | P2 |
| T2 | Agents View（roster 購読・attach 切替）+ Tree View（branch/fork/clone 選択 UI） | P3 |
| T3 | 磨き: ツール呼び出し折りたたみ・diff 表示・`?` ヘルプ自動生成・`agentpit` 引数なしの既定を TUI へ昇格（→ Q8） | T2 |

### 11.5 既存 UX 計画との整理

- **B3（console 版 /tree）は廃止** → T2 の Tree View に一本化。データ層（ツリー構築・
  branch API）のみ P1 で先行実装。
- **B1 は分割**: 非対話（プレーン表 + `--json`）を残し、対話一覧は Agents View。
- **A2/A3 は TUI に吸収されつつ独立でも維持**（ワンショット経路用）。
- rustyline REPL の去就・既定コマンド交代の時期は Q8。

## 12. 未決事項（レビューで決めたい）

- **Q1 (Windows)**: CI/リリースに Windows が無いため **UDS 一本・named pipe 抽象なし**で
  設計した。Windows 配布の計画が近くにあるなら、ソケットパス解決だけ1関数に集約して
  おく（している）ので差し替え可能だが、trait 抽象は今は切らない。この方針で良いか。
- **Q2 (記録スコープ)**: REPL は**常時記録**（オフ設定なし、ファイルは軽い）+
  ワンショット（rescue/review 等）は**記録しない**を既定にした。ワンショットも
  `--session <id>` で既存セッションに紐付ける拡張は P2 以降。この線引きで良いか。
- **Q3 (継続の既定)**: native resume 対応は claude/codex から。antigravity/opencode は
  compose フォールバック開始（実装時に各 CLI の resume 実挙動を確認して昇格）。良いか。
- **Q4 (eviction 既定値)**: 30分（引き継ぎの想定値。リプレイが軽いので短くて損がない）
  vs 90分（prime 実装の既定）。**30分を提案**。
- **Q5 (フル TUI)**: **決定（2026-08-08）: スコープ内**。§11 の T1〜T3 として実装する
  （当初の「console ベースで様子見」は撤回。ratatui 委譲により当初懸念の再実装工数が
  消えたため）。
- **Q8 (REPL の去就と既定コマンド)**: TUI 安定後、rustyline REPL を廃止するか
  dumb terminal / SSH 用の代替として残すか。`agentpit` 引数なしの既定を現行の
  cliclack メニューから TUI へ交代する時期（T3 を提案）。
- **Q6 (Orchestration REPL の優先度)**: R1〜R3 は P1〜P2 依存。P3（状態機械）まで
  終えてから着手か、P2 完了時点で R1 に並行着手か。§10.10 の `LlmCompletion` trait
  だけは依存がないため、早期に欲しければ P1 と並行で先行実装できる。
- **Q7 (REPL からの dispatch 既定隔離レベル)**: 最も保守的（codex: read-only /
  claude: acceptEdits なし）を既定とし、書き込みはセル側の明示指定
  （`{ write: true }` / `{ isolated: true }`）で解放する提案。安全側だが
  オーケストレーションの手数は増える。この既定で良いか。
