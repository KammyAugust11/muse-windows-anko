# Muse for Windows — Rust + WebView2 (WebUI) 版 by あんこ（anko.exe）

Edge の `--app` ショートカットとは違う、**単体で動く本物の .exe** です（Rust 製、約2MB）。
アイコンはあんこの顔。タスクトレイにあんこが常駐します。実行ファイル名も `anko.exe` です（中身は Muse）。

## 何が違うの？（Edgeアプリ風ランチャーとの比較）

| | Edge `--app` ランチャー | この Rust 版 |
|---|---|---|
| 実体 | Edge のショートカット | 単体 `anko.exe`（Rust ネイティブ） |
| ブラウザ依存 | Edge のプロファイル・更新に依存 | WebView2 ランタイムのみ（Win11 標準搭載） |
| ログインデータ | Edge のプロファイルと混ざる | `%APPDATA%\Muse\WebView2` に専用分離 |
| 常駐 | なし（ウィンドウを閉じたら終わり） | **タスクトレイ常駐**。×ボタンはトレイに隠すだけ |
| 自動起動 | なし | Windows サインイン時にトレイへ自動起動（切替可） |
| 二重起動 | ウィンドウが増える | 名前付き Mutex で防止。2回目の起動は、常駐中のウィンドウを前面に出すだけ |
| ウィンドウ状態 | Edge 任せ | 位置・サイズ・最大化を保存して次回復元（外したモニターの位置には戻さない） |
| 外部リンク | アプリ内に読み込まれる | http(s)・mailto は既定のブラウザで開く（Muse/ログイン関連だけ内部）。それ以外の種類のリンクは開かない |
| MCP | なし | 手元の MCP サーバーのツールを実行して、結果を入力欄へ貼り付け |
| フルスクリーン | ブラウザ任せ | F11 対応（JS → IPC → ネイティブ切替） |

## 常駐まわり

- ウィンドウの × を押しても終了しません。タスクトレイ（通知領域）に隠れます
- トレイアイコンの左クリックで表示/非表示を切り替え
- トレイの右クリックメニュー: 「Muse を開く」「ホームへ戻る」「再読み込み」「MCP ツール…」「Windows 起動時に自動起動（チェック式）」「終了」
- 本当に終わらせたいときはメニューの「終了」
- `anko.exe --tray` で起動すると、最初から隠れた状態で常駐します（自動起動はこの形）
- 常駐中にもう一度 `anko.exe` を起動すると、隠れているウィンドウが前面に出ます

## 使い方

1. ZIP を展開する
2. `Install.cmd` をダブルクリック
   → `%LOCALAPPDATA%\Programs\Muse` に入り、デスクトップとスタートメニューに「Muse」ができて、サインイン時の自動起動も登録されます
3. 初回は iPhone / Android と同じアカウントでログインしてください（会話・記憶はそのまま引き継がれます）
4. ※ `WebView2Loader.dll` が同梱されている場合（GNU ツールチェーンでビルドした版）は、`anko.exe` と同じフォルダに置いてください。Windows 上で普通にビルドした版（MSVC）は `anko.exe` 単体で動きます

入れ直すときも `Install.cmd` をもう一度実行するだけです（起動中の Muse は自動で終了させてから上書きします）。

## 動作条件

- Windows 10（更新済み）/ Windows 11
- WebView2 Runtime（Windows 11 には標準搭載。無い場合は Microsoft 公式から無料で入ります）
- インターネット接続

## ビルド方法（ソースから）

Windows 実機なら一番簡単:

```bat
cargo build --release
:: 出力: target\release\anko.exe
:: これを Install.cmd と同じフォルダにコピーしてから Install.cmd を実行
cargo test
```

GitHub Actions（`.github/workflows/build.yml`）が `main` への push ごとにテストとビルドを行い、`anko.exe`・`Install.cmd`・`icon.ico` 入りの一式を成果物として残します。`v1.2.0` のようなタグを push すると、同じ一式の ZIP が Releases に載ります。

Linux からクロスコンパイルする場合（あんこがやった方法）:

```sh
rustup target add x86_64-pc-windows-gnu
cargo install cargo-zigbuild   # zig も必要
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

## MCP クライアント機能

トレイメニューの「MCP ツール…」（または `anko.exe --mcp`）で、手元の [MCP](https://modelcontextprotocol.io/) サーバーのツールを使う画面が開きます。

1. 「設定ファイルを開く」で `%APPDATA%\Muse\mcp.json` を開き、サーバーを登録して保存（他の MCP 対応アプリと同じ `mcpServers` 形式）

   ```json
   {
     "mcpServers": {
       "filesystem": {
         "command": "npx",
         "args": ["-y", "@modelcontextprotocol/server-filesystem", "C:\\Users\\you\\Documents"]
       }
     }
   }
   ```

2. 「再読み込み」を押すとサーバーが起動し、ツールの一覧が出ます
3. ツールを選び、引数（JSON）を入れて「実行」
4. 結果を「Muse の入力欄に貼り付け」（送信はしません。内容を確認してから自分で送信）または「コピー」

できること・できないこと:

- 対応しているのは **command で起動する stdio サーバーのツール**だけです。URL で接続する HTTP サーバー、リソース、プロンプトには未対応です
- **Muse が自分でツールを呼ぶことはできません。** Muse の AI は Meta のサーバー側で動いていて、このアプリからツールを渡す手段が無いためです。ツールを実行するのは常に、この画面で「実行」を押した人です
- 安全のため、Muse の Web ページ側からは MCP に一切触れられない作りにしています（MCP の画面はアプリに埋め込んだ別ウィンドウで、Web のページを読み込みません）
- `mcp.json` に書いたコマンドは、あなたの権限でこの PC 上で実行されます。信頼できるサーバーだけ登録してください
- サーバーのプロセスは、アプリの終了時（異常終了も含む）に子プロセスごと止まります

## 中身

- `src/main.rs` — 本体。tao でネイティブウィンドウ、wry で WebView2、tray-icon でトレイ常駐
- `src/mcp.rs` / `src/mcp_ui.html` — MCP クライアント（JSON-RPC を標準入出力でやり取り）と、その画面
- Win32 の細部（Mutex、自動起動レジストリ、通知、子プロセスをまとめて止めるジョブ）は生成バインディングを使わず `extern "system"` で直接宣言 — 依存を小さく保つため
- 依存: tao / wry / tray-icon / serde（ウィンドウ状態）/ open（外部リンク）/ image（アイコン読込）
- テレメトリなし。常駐中も通信は Muse の Web 本体だけです（MCP サーバーを登録した場合は、そのサーバーが行う通信が加わります）
- 環境変数 `MUSE_DATA_DIR` を指定すると、データの保存先（ログイン情報・ウィンドウ状態・`mcp.json`）を `%APPDATA%\Muse` から変えられます

※ 非公式のラッパーです。Muse は Meta のサービスで、ログイン情報はこのアプリではなく WebView2 の専用プロファイル内に保存されます。`WebView2Loader.dll` は Microsoft の再頒布可能なローダーです。
