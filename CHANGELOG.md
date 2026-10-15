# Changelog

破壊的変更を入れたら必ずここに書き、Cargo.toml の version を上げること
（semver。1.0 未満は minor が breaking の扱い）。消費側は rev pin なので、
rev を進める判断材料はこのファイルと git log になる。

## 0.1.0 (未リリース)

- feature: `ALTER TABLE ... DROP CONSTRAINT` を機械変換の対象に追加 (kadai からの依頼。
  CHECK 制約を落としてアプリ層 enum に一本化する定型変更が diff → apply の通常サイクルに乗る)。
  文の形は素通しだが、`diff` が baseline (migrations/ の再生結果) の `pg_constraint` で
  制約の種別を引き、**CHECK (`contype = 'c'`) だけ**通す。PRIMARY KEY / UNIQUE は DSQL で
  drop が通るか未実測のため fail-loud (実測後に allowlist を広げる予定)。baseline に無い
  制約名も fail-loud (migrations と schema.sql の不整合の検出)。`ADD CONSTRAINT` は従来どおり
  fail-loud。`--enable-drop` のゲートには載せない (データ喪失がない)。
  公開 API は追加のみ: `dsqlize::dsqlize_with_baseline(db, statements)` (変換 + catalog 検証) /
  `parse_drop_constraint` / `check_drop_constraint_type`。DB を触らない `dsqlize::dsqlize` は
  従来どおり DROP CONSTRAINT を Err にする (種別を検証できない経路で通さない。lib 利用者は
  `dsqlize_with_baseline` に乗り換えると通る)。機械変換する DROP CONSTRAINT はテーブル名の
  schema 修飾が必須 (psqldef は常に付ける。無修飾を public と見なすと search_path 次第で
  別スキーマの同名制約を落としうるため)
- test: `diff` の結合テスト (`tests/diff_drop_constraint.rs`) を追加。ローカル PG + psqldef が
  要るため `DSQL_SCHEMA_TEST_PG_HOST` を設定したときだけ走る (未設定なら skip)

- 運用: 開発体制を「消費側 VM からの共同編集」から「この repo 専用環境で feedback を
  受けて開発する」形に変更。変更依頼は kadai の dsql-schema-migrate 宛 feedback へ。
  crate の API / CLI には変更なし

- deps: `aws-sdk-dsql` を `default-features = false` + `default-https-client` / `rt-tokio`
  に変更。default の `rustls` feature が legacy TLS スタック (rustls 0.21 /
  rustls-webpki 0.101、既知の脆弱性 advisory あり) を依存に持ち込んでいた。
  消費側も同じ指定にしないと feature unification で legacy スタックが残る点に注意
- **CLI 挙動変更**: `--pg-user` の env を汎用の `USER` から crate 固有の
  `DSQL_SCHEMA_PG_USER` に変更。旧実装は `USER` (VM のログイン名 = `forge` 等) を
  PG ユーザーとして採用してしまい、既定値 `postgres` が事実上機能しなかった
  (消費側が全レシピで `--pg-user postgres` を渡す必要があった)。**`$USER` と PG
  ユーザーが一致する運用に依存していた場合は挙動が変わる** (以後は `postgres` が
  既定。従来どおりログイン名で繋ぐなら `DSQL_SCHEMA_PG_USER=$USER` か `--pg-user`)。
  この変更で消費側の justfile から `--pg-user postgres` の引き回しを外せる
- refactor: 文の分類 (CREATE TABLE / CREATE INDEX / …) を `StatementKind` に一元化。
  transform / normalize_export / target_table が個別に prefix 判定しており、
  判定のズレがそのままバグになる構造だった (上記の偽陰性バグがこの型)
- refactor: `export --check` のドリフトを `std::process::exit(2)` の直呼びから
  `DriftDetected` エラーに変更し、exit code への変換をエントリポイント
  (`run_cli`) に集約。lib のコードがプロセスを殺さなくなる
  (**exit 2 の契約は維持** = CI はそのまま)
- fix: `normalize_export` の運用テーブル除去を「文の対象テーブル名の一致」に修正。
  旧実装は文全体の substring 判定だったため、名前に `schema_migrations` を含むだけの
  別テーブル (例: `user_schema_migrations_log`) や同名カラムを持つテーブルまで黙って
  除去され、ドリフト検査が偽陰性 (ドリフト見逃し) になり得た
- fix: applier が SQL に埋め込む version (ファイル名) をエスケープするように
  (`'` を含むファイル名で文が壊れていた)
- fix: 「履歴の記録 → 進捗行の削除」の境界でクラッシュすると
  `schema_migration_progress` に stale 行が残り続けていた。次回の apply で掃除する
- 防御強化: `dsqlize` の serial 検出を大文字小文字非依存にし、シーケンス展開形
  `nextval()` も fail-loud の対象に追加 (手書き schema.sql の書式が psqldef を
  素通りするケース向け)
- 防御強化: `rebuild-lab` / `diff` の DB 名を識別子 (英字/_ 始まり + 英数字/_) に
  制限 (DROP/CREATE DATABASE への埋め込みのため)
- fix: psqldef 起動失敗のメッセージが消費側に存在しない `just setup-psqldef` を
  案内していた (README の導入手順への案内に変更)
- fix: `normalize_export` が運用テーブル `schema_migration_progress` を除去して
  いなかった (apply 実行済みクラスタへの `export --dsql --check` が必ず偽ドリフトに
  なる。kadai の sandbox 実機で発見)
- 初版。kikiwake の workspace crate からの切り出し
  - `diff`: schema.sql と「migrations/ の再生結果 (baseline)」の差分から
    DSQL 方言の migration を生成 (psqldef --dry-run を機械変換。変換不能は fail-loud)
  - `apply`: migrations/*.sql を 1 文ずつ autocommit で適用。文単位チェックポイント
    (`schema_migration_progress`) により部分失敗から再開できる
  - `rebuild-lab` / `export` (--check でドリフト検査) / `status`
  - lib + bin 両対応。`run_cli()` を呼ぶ薄い bin を消費側に置く想定
