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

/// 文の種別。
///
/// 分類はここに一元化する。transform（方言変換）・normalize_export（正規形化）・
/// target_table（対象テーブル抽出）がそれぞれ独自に prefix 判定すると、判定のズレが
/// そのままバグになる（運用テーブル除去が substring 判定でドリフト検査を偽陰性に
/// していた事例がある）。
///
/// 判定が大文字小文字を区別するのは、psqldef が生成する DDL が常にこの表記の
/// ためで、それ以外（手書きの小文字 DDL 等）は Other = fail-loud に倒す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    CreateTable,
    /// `CREATE INDEX` / `CREATE UNIQUE INDEX`
    CreateIndex,
    Drop,
    AlterTableAddColumn,
    /// 機械変換の対象外（手書きへ誘導する）
    Other,
}

impl StatementKind {
    fn of(stmt: &str) -> Self {
        if stmt.starts_with("CREATE TABLE") {
            Self::CreateTable
        } else if stmt.starts_with("CREATE INDEX") || stmt.starts_with("CREATE UNIQUE INDEX") {
            Self::CreateIndex
        } else if stmt.starts_with("DROP TABLE") || stmt.starts_with("DROP INDEX") {
            Self::Drop
        } else if stmt.starts_with("ALTER TABLE") && stmt.contains("ADD COLUMN") {
            Self::AlterTableAddColumn
        } else {
            Self::Other
        }
    }
}

/// 1 文を DSQL 方言へ変換する。
fn transform(stmt: &str) -> anyhow::Result<String> {
    let stmt = stmt.trim();
    match StatementKind::of(stmt) {
        StatementKind::CreateTable => {
            ensure_dsql_table(stmt)?;
            Ok(strip_using_btree(stmt))
        }
        StatementKind::CreateIndex => Ok(to_async_index(&strip_using_btree(stmt))),
        StatementKind::Drop => Ok(stmt.to_owned()),
        StatementKind::AlterTableAddColumn => {
            ensure_plain_add_column(stmt)?;
            Ok(stmt.to_owned())
        }
        // psqldef は新規テーブルの CREATE TABLE を schema.sql の記述のまま echo するため、
        // 小文字で書かれた DDL もここに落ちる（分類は psqldef の生成表記に合わせている）
        StatementKind::Other => bail!(
            "dsql-schema-migrate が機械変換できない文です。DSQL の DDL 制約を確認し、\
             必要なら migration を手書きしてください (schema.sql の DDL は psqldef の\
             表記 = キーワード大文字で書いてください):\n{stmt}"
        ),
    }
}

/// `CREATE [UNIQUE] INDEX` を DSQL の非同期形にする（DSQL のインデックス作成は ASYNC のみ）。
/// UNIQUE 側が先に一致した場合、2 つ目の replacen は対象が残らず no-op になる。
fn to_async_index(stmt: &str) -> String {
    stmt.replacen("CREATE UNIQUE INDEX ", "CREATE UNIQUE INDEX ASYNC ", 1)
        .replacen("CREATE INDEX ", "CREATE INDEX ASYNC ", 1)
}

/// ADD COLUMN が DSQL の許す形（nullable・制約なし）であることを確認する。
/// それ以外はテーブル再作成パターンでの手書きへ誘導する。
fn ensure_plain_add_column(stmt: &str) -> anyhow::Result<()> {
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
    Ok(())
}

/// CREATE TABLE が DSQL 非対応の要素を含まないことを確認する。
/// キーワードの大文字小文字は入力（手書き schema.sql の書式が psqldef を素通りする）に
/// 依存するため、小文字化してから判定する。nextval() は serial をシーケンス既定値に
/// 展開した形（DSQL はシーケンス自体が使えない）。
fn ensure_dsql_table(stmt: &str) -> anyhow::Result<()> {
    let lower = stmt.to_lowercase();
    for forbidden in [
        "foreign key",
        "references ",
        " serial",
        " bigserial",
        " smallserial",
        "nextval(",
    ] {
        if lower.contains(forbidden) {
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
    // 変換（catalog リーク除去）→ 運用テーブル除去の順。DSQL の `_index` リークを
    // 先に剥がすことで、運用テーブルへの index も対象テーブル名の一致で落とせる
    let statements: Vec<String> = split_statements(&strip_using_btree(export))
        .into_iter()
        .map(|s| match StatementKind::of(&s) {
            StatementKind::CreateTable => sort_table_body(&s),
            StatementKind::CreateIndex if from_dsql => s.replacen("_index (", " (", 1),
            _ => s,
        })
        .filter(|s| !is_internal_table_statement(s))
        .collect();
    let mut out = statements.join(";\n\n");
    if !out.is_empty() {
        out.push_str(";\n");
    }
    out
}

/// applier が管理用に作るテーブル（schema.sql には書かれないため export から除去する）。
const INTERNAL_TABLES: [&str; 2] = ["schema_migrations", "schema_migration_progress"];

/// 文の対象テーブルが applier の運用テーブルなら true。
/// 名前の部分一致では判定しない（`user_schema_migrations_log` のような
/// 名前が偶然含むだけの別テーブルや、運用テーブル名と同名のカラムを持つ
/// テーブルを巻き込んで、ドリフト検査を偽陰性にしないため）。
fn is_internal_table_statement(stmt: &str) -> bool {
    target_table(stmt).is_some_and(|table| INTERNAL_TABLES.contains(&table.as_str()))
}

/// 文の対象テーブル名（CREATE TABLE の対象 / CREATE INDEX の ON 先）を、
/// schema 修飾と引用符を剥がした素の名前で取り出す。対象を持たない文は None。
fn target_table(stmt: &str) -> Option<String> {
    let rest = match StatementKind::of(stmt) {
        StatementKind::CreateTable => stmt.strip_prefix("CREATE TABLE ")?,
        StatementKind::CreateIndex => stmt.split(" ON ").nth(1)?,
        _ => return None,
    };
    let token = rest.split(|c: char| c.is_whitespace() || c == '(').next()?;
    let name = token.rsplit('.').next()?.trim_matches('"');
    (!name.is_empty()).then(|| name.to_owned())
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
        // キーワードは大文字小文字を問わず検出する（手書き schema.sql が素通りする形）
        assert!(dsqlize(&stmts(&["CREATE TABLE a (id SERIAL PRIMARY KEY)"])).is_err());
        // serial のシーケンス展開形（nextval）も止める
        assert!(
            dsqlize(&stmts(&[
                "CREATE TABLE a (id integer NOT NULL DEFAULT nextval('a_id_seq'::regclass))",
            ]))
            .is_err()
        );
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
    fn statement_kind_は文を分類する() {
        use StatementKind::*;
        for (stmt, want) in [
            ("CREATE TABLE \"public\".\"a\" (\"id\" uuid)", CreateTable),
            ("CREATE INDEX i ON public.a (c)", CreateIndex),
            ("CREATE UNIQUE INDEX u ON public.a (c)", CreateIndex),
            ("DROP TABLE \"public\".\"a\"", Drop),
            ("DROP INDEX i", Drop),
            (
                "ALTER TABLE \"public\".\"a\" ADD COLUMN \"c\" text",
                AlterTableAddColumn,
            ),
            ("ALTER TABLE a ALTER COLUMN c TYPE text", Other),
            // psqldef の生成表記 (キーワード大文字) 以外は Other = fail-loud に倒す
            ("create table a (id uuid)", Other),
        ] {
            assert_eq!(StatementKind::of(stmt), want, "分類が想定と違う: {stmt}");
        }
    }

    #[test]
    fn normalize_export_は運用テーブル名を含むだけの別テーブルを除去しない() {
        // 名前が部分一致するだけのユーザーテーブルは残す（旧実装は substring 判定で
        // 黙って落としており、ドリフト検査が偽陰性になっていた）
        let export = "CREATE TABLE \"public\".\"user_schema_migrations_log\" (\n    \"id\" uuid NOT NULL\n);\n";
        assert_eq!(normalize_export(export, false), export);

        // 運用テーブル名と同名の「カラム」を持つテーブルも残す
        let export =
            "CREATE TABLE \"public\".\"a\" (\n    \"schema_migrations\" text NOT NULL\n);\n";
        assert_eq!(normalize_export(export, false), export);

        // 運用テーブルへの index は対象テーブル名の一致で除去する
        let export = "CREATE INDEX idx ON public.schema_migration_progress (version);\n\nCREATE INDEX idx_a ON public.a (id);\n";
        assert_eq!(
            normalize_export(export, false),
            "CREATE INDEX idx_a ON public.a (id);\n"
        );

        // DSQL の catalog リーク（`<table>_index`）越しでも運用テーブルを除去できる
        let export = "CREATE UNIQUE INDEX pkey ON public.schema_migrations_index (version);\n";
        assert_eq!(normalize_export(export, true), "");
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
