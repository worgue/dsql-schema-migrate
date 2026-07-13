//! psqldef の出力（素の PG 方言）を DSQL 方言へ機械変換する。
//!
//! 変換できるのは DSQL がサポートする形の DDL だけ。それ以外（制約付き ADD COLUMN、
//! FOREIGN KEY、SERIAL 等）は fail-loud でエラーにし、手書き migration
//! （新テーブル作成 → データコピー → リネームのテーブル再作成パターン等）へ切り替えてもらう。
//! 判定は保守的で、疑わしい文は誤検知側に倒す（誤検知なら手書きで通せる）。

use anyhow::bail;

use crate::applier::split_statements;

/// `psqldef --dry-run` の出力を分解した結果。
pub struct DryRun {
    /// 適用対象の文（素の PG 方言、`;` なし）
    pub statements: Vec<String>,
    /// `--enable-drop` なしのため psqldef がスキップした破壊的変更（`-- Skipped:` 行）
    pub skipped: Vec<String>,
}

/// `psqldef --dry-run` の出力からラッパー（`-- dry run --` / BEGIN / COMMIT）を剥がす。
pub fn parse_dry_run(output: &str) -> DryRun {
    let mut skipped = Vec::new();
    let mut body = String::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "-- dry run --"
            || trimmed == "-- Nothing is modified --"
            || trimmed == "BEGIN;"
            || trimmed == "COMMIT;"
        {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("-- Skipped: ") {
            skipped.push(rest.to_owned());
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    DryRun {
        statements: split_statements(&body),
        skipped,
    }
}

/// 素の PG 方言の文リストを DSQL 方言へ変換する。変換不能な文があればエラー。
pub fn dsqlize(statements: &[String]) -> anyhow::Result<Vec<String>> {
    statements.iter().map(|s| transform(s)).collect()
}

/// 1 文を DSQL 方言へ変換する。
fn transform(stmt: &str) -> anyhow::Result<String> {
    let stmt = stmt.trim();

    if stmt.starts_with("CREATE TABLE") {
        ensure_dsql_table(stmt)?;
        return Ok(strip_using_btree(stmt));
    }
    if stmt.starts_with("CREATE UNIQUE INDEX") || stmt.starts_with("CREATE INDEX") {
        // DSQL のインデックス作成は非同期のみ（CREATE INDEX ASYNC）
        return Ok(strip_using_btree(stmt)
            .replacen("CREATE UNIQUE INDEX ", "CREATE UNIQUE INDEX ASYNC ", 1)
            .replacen("CREATE INDEX ", "CREATE INDEX ASYNC ", 1));
    }
    if stmt.starts_with("DROP TABLE") || stmt.starts_with("DROP INDEX") {
        return Ok(stmt.to_owned());
    }
    if stmt.starts_with("ALTER TABLE") && stmt.contains("ADD COLUMN") {
        // DSQL の ADD COLUMN は nullable・制約なしのみ。それ以外はテーブル再作成（手書き）
        for forbidden in [
            "NOT NULL",
            "DEFAULT",
            "REFERENCES",
            "UNIQUE",
            "PRIMARY KEY",
            "CHECK",
        ] {
            if stmt.contains(forbidden) {
                bail!(
                    "DSQL は制約付き ADD COLUMN ({forbidden}) を適用できません。\
                     テーブル再作成パターン (新テーブル作成 → データコピー → リネーム) で\
                     手書きしてください:\n{stmt}"
                );
            }
        }
        return Ok(stmt.to_owned());
    }

    bail!(
        "dsql-schema-migrate が機械変換できない文です。DSQL の DDL 制約を確認し、\
         必要なら migration を手書きしてください:\n{stmt}"
    );
}

/// CREATE TABLE が DSQL 非対応の要素を含まないことを確認する。
fn ensure_dsql_table(stmt: &str) -> anyhow::Result<()> {
    for forbidden in [
        "FOREIGN KEY",
        "REFERENCES ",
        " serial",
        " bigserial",
        " smallserial",
    ] {
        if stmt.contains(forbidden) {
            bail!(
                "DSQL は {} を使えません (FK は参照カラムのみ・連番は UUID 等で代替)。\
                 schema.sql を修正してください:\n{stmt}",
                forbidden.trim()
            );
        }
    }
    Ok(())
}

/// `USING btree` を除去する（DSQL 方言・正規形の両方で不要）。
fn strip_using_btree(stmt: &str) -> String {
    stmt.replace(" USING btree", "")
}

/// `psqldef --export` の出力を schema.sql の正規形へ変換する。
///
/// - `USING btree` を除去（psqldef 自身の generic parser がカラム名 `text` 等で
///   これに躓く問題も避けられる。除去後の INDEX ASYNC が DSQL で受理されることは実証済み）
/// - 運用テーブル `schema_migrations` / `schema_migration_progress` を除去
///   （DSQL からの export でのドリフト検査用。applier が作る管理テーブルは
///   schema.sql に書かれないため、残すと偽ドリフトになる）
/// - CREATE TABLE のカラム行をアルファベット順にソート（psqldef の export 順は
///   DB の物理カラム順に依存して揺れるため、正規形はこちらで確定させる。
///   ラウンドトリップ・ドリフト検査のバイト一致はこのソートが前提）
/// - `from_dsql=true` では DSQL catalog のリーク（index の対象テーブルが
///   `<table>_index` と報告される）を除去する（実クラスタとのドリフト検査用。
///   `_index` で終わる名前のテーブルとは共存できないので付けないこと）
/// - 文の間は空行 1 つ、末尾は改行 1 つに揃える
pub fn normalize_export(export: &str, from_dsql: bool) -> String {
    let statements: Vec<String> = split_statements(&strip_using_btree(export))
        .into_iter()
        .filter(|s| !s.contains("schema_migrations") && !s.contains("schema_migration_progress"))
        .map(|s| {
            if s.starts_with("CREATE TABLE") {
                sort_table_body(&s)
            } else if from_dsql
                && (s.starts_with("CREATE INDEX") || s.starts_with("CREATE UNIQUE INDEX"))
            {
                s.replacen("_index (", " (", 1)
            } else {
                s
            }
        })
        .collect();
    let mut out = statements.join(";\n\n");
    if !out.is_empty() {
        out.push_str(";\n");
    }
    out
}

/// CREATE TABLE 本体の行（カラム定義・制約）を並べ替える。
/// カラム行はアルファベット順、CONSTRAINT / CHECK 等の制約行はその後ろに回す。
/// 定義は 1 行 1 要素である前提（psqldef export の形式）。
fn sort_table_body(stmt: &str) -> String {
    let mut lines = stmt.lines();
    let Some(header) = lines.next() else {
        return stmt.to_owned();
    };
    let body: Vec<&str> = lines.collect();
    let Some((last, defs)) = body.split_last() else {
        return stmt.to_owned();
    };
    let mut columns: Vec<String> = Vec::new();
    let mut constraints: Vec<String> = Vec::new();
    for def in defs {
        let entry = def.trim_end().trim_end_matches(',').to_owned();
        if entry.trim_start().starts_with('"') {
            columns.push(entry);
        } else {
            constraints.push(entry);
        }
    }
    columns.sort();
    constraints.sort();
    columns.extend(constraints);
    let mut out = format!("{header}\n");
    let total = columns.len();
    for (i, entry) in columns.iter().enumerate() {
        out.push_str(entry);
        if i + 1 < total {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(last);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_dry_run_はラッパーを剥がす() {
        let output = "-- dry run --\nBEGIN;\nALTER TABLE \"public\".\"a\" ADD COLUMN \"c\" text;\n-- Skipped: DROP TABLE \"public\".\"b\";\nCOMMIT;\n";
        let parsed = parse_dry_run(output);
        assert_eq!(
            parsed.statements,
            stmts(&["ALTER TABLE \"public\".\"a\" ADD COLUMN \"c\" text"])
        );
        assert_eq!(parsed.skipped, stmts(&["DROP TABLE \"public\".\"b\";"]));
    }

    #[test]
    fn parse_dry_run_は変更なしを空にする() {
        let parsed = parse_dry_run("-- Nothing is modified --\n");
        assert!(parsed.statements.is_empty());
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn dsqlize_は_index_を_async_化して_using_btree_を除去する() {
        let result = dsqlize(&stmts(&[
            "CREATE INDEX idx_a ON public.a USING btree (c)",
            "CREATE UNIQUE INDEX a_key ON public.a USING btree (c)",
        ]))
        .unwrap();
        assert_eq!(result[0], "CREATE INDEX ASYNC idx_a ON public.a (c)");
        assert_eq!(result[1], "CREATE UNIQUE INDEX ASYNC a_key ON public.a (c)");
    }

    #[test]
    fn dsqlize_は_create_table_と素の_add_column_を通す() {
        let result = dsqlize(&stmts(&[
            "CREATE TABLE \"public\".\"a\" (\"id\" uuid NOT NULL, CONSTRAINT a_pkey PRIMARY KEY (\"id\"))",
            "ALTER TABLE \"public\".\"a\" ADD COLUMN \"note\" text",
            "DROP TABLE \"public\".\"b\"",
        ]))
        .unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn dsqlize_は制約付き_add_column_で止まる() {
        let err = dsqlize(&stmts(&[
            "ALTER TABLE \"public\".\"a\" ADD COLUMN \"n\" integer NOT NULL DEFAULT 1",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("テーブル再作成"));
    }

    #[test]
    fn dsqlize_は_fk_と_serial_で止まる() {
        assert!(
            dsqlize(&stmts(&[
                "CREATE TABLE a (id uuid, b_id uuid REFERENCES b (id))",
            ]))
            .is_err()
        );
        assert!(dsqlize(&stmts(&["CREATE TABLE a (id serial PRIMARY KEY)"])).is_err());
        assert!(dsqlize(&stmts(&["ALTER TABLE a ALTER COLUMN c TYPE text"])).is_err());
    }

    #[test]
    fn normalize_export_は_using_btree_と運用テーブルを除去する() {
        let export = "CREATE TABLE \"public\".\"a\" (\n    \"id\" uuid NOT NULL\n);\n\nCREATE TABLE \"public\".\"schema_migrations\" (\n    \"version\" text NOT NULL\n);\n\nCREATE TABLE \"public\".\"schema_migration_progress\" (\n    \"version\" text NOT NULL\n);\n\nCREATE INDEX idx_a ON public.a USING btree (id);\n";
        let normalized = normalize_export(export, false);
        assert_eq!(
            normalized,
            "CREATE TABLE \"public\".\"a\" (\n    \"id\" uuid NOT NULL\n);\n\nCREATE INDEX idx_a ON public.a (id);\n"
        );
        // 正規形は冪等（もう一度通しても変わらない）
        assert_eq!(normalize_export(&normalized, false), normalized);
    }

    #[test]
    fn normalize_export_は_dsql_catalog_の_index_suffix_を除去する() {
        let export = "CREATE UNIQUE INDEX a_key ON public.a_index (slug);\n";
        assert_eq!(
            normalize_export(export, true),
            "CREATE UNIQUE INDEX a_key ON public.a (slug);\n"
        );
        // ローカル PG からの export では除去しない
        assert_eq!(normalize_export(export, false), export);
    }

    #[test]
    fn normalize_export_はカラム行をソートし制約を後ろに回す() {
        let export = "CREATE TABLE \"public\".\"a\" (\n    \"b\" text NOT NULL,\n    CONSTRAINT a_pkey PRIMARY KEY (\"id\"),\n    \"id\" uuid NOT NULL,\n    \"a\" integer\n);\n";
        let normalized = normalize_export(export, false);
        assert_eq!(
            normalized,
            "CREATE TABLE \"public\".\"a\" (\n    \"a\" integer,\n    \"b\" text NOT NULL,\n    \"id\" uuid NOT NULL,\n    CONSTRAINT a_pkey PRIMARY KEY (\"id\")\n);\n"
        );
        // カラム順が違う DB からの export でも同じ正規形になる
        let export_reordered = "CREATE TABLE \"public\".\"a\" (\n    \"id\" uuid NOT NULL,\n    \"a\" integer,\n    \"b\" text NOT NULL,\n    CONSTRAINT a_pkey PRIMARY KEY (\"id\")\n);\n";
        assert_eq!(normalize_export(export_reordered, false), normalized);
    }
}
