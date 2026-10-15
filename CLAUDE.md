# CLAUDE.md

このリポジトリは日本語が基準です。コメント・ドキュメント・コミットメッセージは
日本語で記述してください。

## このリポジトリの位置づけ

Aurora DSQL 向けスキーマ管理 crate。この repo 専用の開発環境で、消費側プロジェクト
（kikiwake / kadai 等）からの feedback を受けて開発します。消費側の VM からこの repo を
直接編集することはしません。

## 開発の運用ルール

- 不具合報告・機能要望・変更依頼は、kadai の dsql-schema-migrate 宛 feedback で受ける。
  依頼の意図に疑問があれば、実装前に送り手のプロジェクトへ feedback で確認する
- ブランチは `dev` が既定。LLM は `dev` へ commit して push してよい。
  `main` / `stg` / `prod` への昇格は人間が行う（force push・ブランチ削除は禁止）
- 消費側プロジェクトは git 依存 + rev pin でこの repo を使う。**変更を入れたら
  CHANGELOG.md に追記**すること（消費側が rev を進めるかの判断材料になる）。
  破壊的変更（公開 API / CLI の互換を壊す変更）は Cargo.toml の version も上げる
- feedback 起点の変更を push したら、依頼元へ対応内容と commit hash を返す
  （rev を進める判断を相手に委ねる）
- 編集後は必ず `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test` を
  通すこと（unit test はローカル PostgreSQL なしで動く）

## 公開リポジトリとしての規律

GitHub 上の**公開 repo**です。以下をコード・コメント・コミットメッセージに書かないこと:

- 組織の内部固有名詞・内部ドキュメントへの参照（`dsql.md §n` のような形の参照を含む。
  制約の内容は参照ではなく直接書く）
- タスク管理 ID・AWS アカウント ID・エンドポイント・secret

プロジェクト名 kikiwake / kadai への言及は可。

kadai の作業コピー（`/kadai`）と `forge.local.toml` / `.envrc.local` は `.gitignore` 済み。
作業コピーには他プロジェクトとやり取りした feedback の本文が入るため、絶対に commit しない
（commit 前に `git status` に現れないことを確認する）。

## 設計の要点

- 信頼の源は消費側プロジェクトの `schema.sql`（素の PG 方言）。この crate 自体は
  プロジェクト固有データを持たない（migrations はディスクから読む）
- diff の baseline は「migrations/ の再生結果」。migrations と schema.sql の不整合を
  diff のたびに検出するための設計なので、baseline を HEAD の schema.sql に変えないこと
- DSQL 方言変換（`dsqlize`）は保守的に fail-loud。変換できない文を黙って通すより、
  止めて手書きに誘導する方針を維持すること
- applier は 1 文ずつ autocommit（DSQL は 1 トランザクション 1 DDL）+
  文単位チェックポイント。「文の適用」と「進捗更新」は別 autocommit になるため、
  ハードクラッシュ時に 1 文だけ再実行されうる制約は既知（lib.rs / applier.rs の doc 参照）
