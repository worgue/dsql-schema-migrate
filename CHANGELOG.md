# Changelog

破壊的変更を入れたら必ずここに書き、Cargo.toml の version を上げること
（semver。1.0 未満は minor が breaking の扱い）。消費側は rev pin なので、
rev を進める判断材料はこのファイルと git log になる。

## 0.1.0 (未リリース)

- 初版。kikiwake の workspace crate からの切り出し
  - `diff`: schema.sql と「migrations/ の再生結果 (baseline)」の差分から
    DSQL 方言の migration を生成 (psqldef --dry-run を機械変換。変換不能は fail-loud)
  - `apply`: migrations/*.sql を 1 文ずつ autocommit で適用。文単位チェックポイント
    (`schema_migration_progress`) により部分失敗から再開できる
  - `rebuild-lab` / `export` (--check でドリフト検査) / `status`
  - lib + bin 両対応。`run_cli()` を呼ぶ薄い bin を消費側に置く想定
