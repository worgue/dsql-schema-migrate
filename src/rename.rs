//! テーブル rename の migration SQL を機械生成する（CLI の `rename-table`）。
//!
//! psqldef の diff はテーブル rename を DROP TABLE + CREATE TABLE と見るため、rename は
//! `diff` では生成できない（データが失われる）。この module は baseline（migrations/ の
//! 再生結果）の catalog から対象テーブルの制約・index を読み、DSQL で通ることが
//! 実測済みの RENAME 形だけで migration を組み立てる:
//!
//! 1. `ALTER TABLE <old> RENAME TO <new>`
//! 2. PRIMARY KEY / UNIQUE 制約の `ALTER TABLE <new> RENAME CONSTRAINT <old>_x TO <new>_x`
//!    （背後の index 名も追随する）
//! 3. 残る index の `ALTER INDEX <old>_x RENAME TO <new>_x`
//!
//! （2026-11-04 に worgue の sandbox クラスターで 1〜3 がこの順で通ることを実測。
//! `RENAME CONSTRAINT` で PK 名を変えると背後の index 名も追随し、行データは保持された）
//!
//! 名前の追従は「`<old>_` で始まる名前の prefix を `<new>_` に置き換える」規則だけ
//! （PG / psqldef の自動命名 `<table>_pkey` / `<table>_<col>_key` と同じ形）。規則に
//! 合わない名前はそのまま残し、生成ファイルの先頭コメントに列挙する。CHECK 制約は
//! `RENAME CONSTRAINT` が DSQL で通るか未実測のため名前を変えない（per-table の
//! 名前空間なので衝突せず、schema.sql 側で旧名を保てば diff は no-op になる）。
//!
//! 受け入れ条件は「出力を migrations/ に置き、schema.sql の該当テーブル・制約・index の
//! 名前を `<new>` に直して `diff` を回すと差分なし」（結合テスト `tests/rename_table.rs`）。

use anyhow::{Context, bail};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

/// baseline から読んだ対象テーブルの制約・index。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableCatalog {
    /// (制約名, `pg_constraint.contype`)。名前順
    pub constraints: Vec<(String, String)>,
    /// PK / UNIQUE 制約の背後ではない index の名前。名前順
    pub indexes: Vec<String>,
}

/// 生成結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlan {
    /// 適用順の文（`;` なし）
    pub statements: Vec<String>,
    /// 名前を変えなかった制約・index についての注記（ファイル先頭のコメントに出す）
    pub notes: Vec<String>,
    /// この plan が新たに作る relation 名（新テーブル名・改名後の制約 / index 名）。
    /// 呼び出し元が baseline との衝突を検査する
    pub new_names: Vec<String>,
}

/// PG の識別子長の上限（バイト）。超える名前は PG が黙って切り詰めるため生成側で止める。
pub const MAX_IDENT_LEN: usize = 63;

/// 識別子を常に二重引用符で括る（内部の `"` は `""`）。
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

fn check_len(name: &str) -> anyhow::Result<()> {
    if name.len() > MAX_IDENT_LEN {
        bail!(
            "識別子 {name} が {MAX_IDENT_LEN} バイトを超えています (PG は黙って切り詰めるため\
             生成しません)。短い名前にするか、該当の制約 / index を手で改名してください"
        );
    }
    Ok(())
}

/// `<old>_x` を `<new>_x` にする。規則に合わなければ None。
fn follow(name: &str, old: &str, new: &str) -> Option<String> {
    let rest = name.strip_prefix(old)?;
    rest.starts_with('_').then(|| format!("{new}{rest}"))
}

/// rename の文を組み立てる（DB を触らない純関数）。
///
/// 順序は実測した順（テーブル → PK / UNIQUE 制約 → 残る index）。各段の中は catalog の
/// 名前順（= 決定的な出力）。
pub fn plan(
    schema: &str,
    old: &str,
    new: &str,
    catalog: &TableCatalog,
) -> anyhow::Result<RenamePlan> {
    if old == new {
        bail!("新旧のテーブル名が同じです: {old}");
    }
    check_len(new)?;
    let mut statements = vec![format!(
        "ALTER TABLE {} RENAME TO {}",
        qualified(schema, old),
        quote_ident(new)
    )];
    let mut notes = Vec::new();
    let mut new_names = vec![new.to_owned()];
    let keep = |what: &str, name: &str| {
        format!(
            "{what} {name} は名前が `{old}_` で始まらないため変えていません (schema.sql でもこの名前のまま残すこと)"
        )
    };

    for (name, contype) in &catalog.constraints {
        let kind = match contype.as_str() {
            "p" => "PRIMARY KEY",
            "u" => "UNIQUE",
            "c" => {
                if follow(name, old, new).is_some() {
                    notes.push(format!(
                        "CHECK 制約 {name} は名前を変えていません (CHECK の RENAME CONSTRAINT は DSQL で\
                         未実測。CHECK 名は per-table なので新テーブル名と衝突しない。schema.sql でも\
                         この名前のまま残すこと)"
                    ));
                }
                continue;
            }
            // PG 18 の NOT NULL 制約。psqldef の export に名前は出ないので触らない
            "n" => continue,
            other => {
                notes.push(format!(
                    "制約 {name} (contype={other}) は種別が対象外のため変えていません"
                ));
                continue;
            }
        };
        match follow(name, old, new) {
            Some(renamed) => {
                check_len(&renamed)?;
                statements.push(format!(
                    "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                    qualified(schema, new),
                    quote_ident(name),
                    quote_ident(&renamed)
                ));
                new_names.push(renamed);
            }
            None => notes.push(keep(&format!("{kind} 制約"), name)),
        }
    }

    for name in &catalog.indexes {
        match follow(name, old, new) {
            Some(renamed) => {
                check_len(&renamed)?;
                statements.push(format!(
                    "ALTER INDEX {} RENAME TO {}",
                    qualified(schema, name),
                    quote_ident(&renamed)
                ));
                new_names.push(renamed);
            }
            None => notes.push(keep("index", name)),
        }
    }
    Ok(RenamePlan {
        statements,
        notes,
        new_names,
    })
}

/// migration ファイルの本文を組み立てる（`diff` の出力と同じ形: 先頭コメント + 空行区切りの文）。
pub fn render(slug: &str, plan: &RenamePlan) -> String {
    let mut out =
        format!("-- {slug} (dsql-schema-migrate rename-table で生成。適用前にレビューすること)\n");
    if !plan.notes.is_empty() {
        out.push_str("-- 注記:\n");
        for note in &plan.notes {
            out.push_str(&format!("-- - {note}\n"));
        }
    }
    out.push('\n');
    let body: Vec<String> = plan.statements.iter().map(|s| format!("{s};\n")).collect();
    out.push_str(&body.join("\n"));
    out
}

/// baseline の catalog から対象テーブルの制約・index を読む。テーブルが無ければ None。
pub async fn load_catalog(
    db: &DatabaseConnection,
    schema: &str,
    table: &str,
) -> anyhow::Result<Option<TableCatalog>> {
    let args = [schema.to_owned().into(), table.to_owned().into()];
    let exists = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS one FROM pg_class r JOIN pg_namespace n ON r.relnamespace = n.oid \
             WHERE n.nspname = $1 AND r.relname = $2 AND r.relkind = 'r'",
            args.clone(),
        ))
        .await
        .context("baseline の pg_class を引けませんでした")?;
    if exists.is_none() {
        return Ok(None);
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT c.conname AS name, c.contype::text AS contype FROM pg_constraint c \
             JOIN pg_class t ON c.conrelid = t.oid \
             JOIN pg_namespace n ON t.relnamespace = n.oid \
             WHERE n.nspname = $1 AND t.relname = $2 ORDER BY c.conname",
            args.clone(),
        ))
        .await
        .context("baseline の pg_constraint を引けませんでした")?;
    let constraints = rows
        .iter()
        .map(|r| Ok((r.try_get("", "name")?, r.try_get("", "contype")?)))
        .collect::<Result<Vec<(String, String)>, sea_orm::DbErr>>()
        .context("pg_constraint の読み取りに失敗")?;
    // FK の conindid は参照先の index を指すので、背後 index の除外は PK / UNIQUE / EXCLUDE に限る
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT i.relname AS name FROM pg_index x \
             JOIN pg_class t ON x.indrelid = t.oid \
             JOIN pg_class i ON x.indexrelid = i.oid \
             JOIN pg_namespace n ON t.relnamespace = n.oid \
             WHERE n.nspname = $1 AND t.relname = $2 \
               AND NOT EXISTS (SELECT 1 FROM pg_constraint c \
                               WHERE c.conindid = i.oid AND c.contype IN ('p', 'u', 'x')) \
             ORDER BY i.relname",
            args,
        ))
        .await
        .context("baseline の pg_index を引けませんでした")?;
    let indexes = rows
        .iter()
        .map(|r| r.try_get("", "name"))
        .collect::<Result<Vec<String>, sea_orm::DbErr>>()
        .context("pg_index の読み取りに失敗")?;
    Ok(Some(TableCatalog {
        constraints,
        indexes,
    }))
}

/// `names` のうち baseline の schema に relation（テーブル / index 等）として既にあるもの。
pub async fn existing_relations(
    db: &DatabaseConnection,
    schema: &str,
    names: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut taken = Vec::new();
    for name in names {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT 1 AS one FROM pg_class r JOIN pg_namespace n ON r.relnamespace = n.oid \
                 WHERE n.nspname = $1 AND r.relname = $2",
                [schema.to_owned().into(), name.clone().into()],
            ))
            .await
            .with_context(|| {
                format!("baseline の relation {schema}.{name} の有無を引けませんでした")
            })?;
        if row.is_some() {
            taken.push(name.clone());
        }
    }
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(constraints: &[(&str, &str)], indexes: &[&str]) -> TableCatalog {
        TableCatalog {
            constraints: constraints
                .iter()
                .map(|(n, t)| (n.to_string(), t.to_string()))
                .collect(),
            indexes: indexes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn 実測した順でテーブル_制約_index_の順に出す() {
        let plan = plan(
            "public",
            "items",
            "things",
            &catalog(
                &[("items_code_key", "u"), ("items_pkey", "p")],
                &["items_slug_key", "items_status_idx"],
            ),
        )
        .unwrap();
        assert_eq!(
            plan.statements,
            [
                "ALTER TABLE \"public\".\"items\" RENAME TO \"things\"",
                "ALTER TABLE \"public\".\"things\" RENAME CONSTRAINT \"items_code_key\" TO \"things_code_key\"",
                "ALTER TABLE \"public\".\"things\" RENAME CONSTRAINT \"items_pkey\" TO \"things_pkey\"",
                "ALTER INDEX \"public\".\"items_slug_key\" RENAME TO \"things_slug_key\"",
                "ALTER INDEX \"public\".\"items_status_idx\" RENAME TO \"things_status_idx\"",
            ]
        );
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(
            plan.new_names,
            [
                "things",
                "things_code_key",
                "things_pkey",
                "things_slug_key",
                "things_status_idx"
            ]
        );
    }

    #[test]
    fn prefix_規則に合わない名前と_check_は変えずに注記する() {
        let plan = plan(
            "public",
            "items",
            "things",
            &catalog(
                &[
                    ("items_pkey", "p"),
                    ("items_status_check", "c"),
                    ("other_check", "c"),
                    ("uq_code", "u"),
                ],
                &["idx_items_created", "itemsx_idx"],
            ),
        )
        .unwrap();
        assert_eq!(
            plan.statements,
            [
                "ALTER TABLE \"public\".\"items\" RENAME TO \"things\"",
                "ALTER TABLE \"public\".\"things\" RENAME CONSTRAINT \"items_pkey\" TO \"things_pkey\"",
            ]
        );
        // CHECK は prefix が合うものだけ注記、UNIQUE / index は規則外を注記
        assert_eq!(plan.notes.len(), 4, "{:?}", plan.notes);
        assert!(plan.notes[0].contains("CHECK 制約 items_status_check"));
        assert!(plan.notes[1].contains("UNIQUE 制約 uq_code"));
        assert!(plan.notes[2].contains("index idx_items_created"));
        assert!(plan.notes[3].contains("index itemsx_idx"));
        assert_eq!(plan.new_names, ["things", "things_pkey"]);
    }

    #[test]
    fn 識別子は常に引用し_内部の二重引用符は重ねる() {
        let plan = plan(
            "public",
            "Mixed Case",
            "a\"b",
            &catalog(&[("Mixed Case_pkey", "p")], &[]),
        )
        .unwrap();
        assert_eq!(
            plan.statements,
            [
                "ALTER TABLE \"public\".\"Mixed Case\" RENAME TO \"a\"\"b\"",
                "ALTER TABLE \"public\".\"a\"\"b\" RENAME CONSTRAINT \"Mixed Case_pkey\" TO \"a\"\"b_pkey\"",
            ]
        );
    }

    #[test]
    fn 同名と長すぎる名前は止める() {
        let err = plan("public", "t", "t", &TableCatalog::default()).unwrap_err();
        assert!(err.to_string().contains("同じ"), "{err}");

        let long = "x".repeat(60);
        let err = plan("public", "t", &long, &catalog(&[("t_pkey", "p")], &[])).unwrap_err();
        assert!(err.to_string().contains("63"), "{err}");
    }

    #[test]
    fn render_は_diff_と同じ形で注記を先頭コメントに出す() {
        let plan = RenamePlan {
            statements: vec!["A".into(), "B".into()],
            notes: vec!["n1".into()],
            new_names: vec![],
        };
        assert_eq!(
            render("rename_a_to_b", &plan),
            "-- rename_a_to_b (dsql-schema-migrate rename-table で生成。適用前にレビューすること)\n\
             -- 注記:\n-- - n1\n\nA;\n\nB;\n"
        );
        let plain = render(
            "s",
            &RenamePlan {
                notes: vec![],
                ..plan
            },
        );
        assert!(!plain.contains("注記"));
        // applier の分割で 2 文になる (先頭コメントは diff の出力と同様に最初の文と同じ塊)
        let split = crate::applier::split_statements(&plain);
        assert_eq!(split.len(), 2, "{split:?}");
        assert!(split[0].ends_with("\nA"), "{split:?}");
        assert_eq!(split[1], "B");
    }
}
