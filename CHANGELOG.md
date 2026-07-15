# Changelog

破壊的変更を入れたら必ずここに書き、Cargo.toml の version を上げること
（semver。1.0 未満は minor が breaking の扱い）。消費側は rev pin なので、
rev を進める判断材料はこのファイルと git log になる。

## 0.1.0 (未リリース)

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
