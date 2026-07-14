//! migrations/*.sql の適用 runner。
//!
//! - 各ステートメントは個別実行（DSQL は DDL 1 文/txn のため tx で括らない）。
//! - **文単位チェックポイント**: 各文の成功ごとに `schema_migration_progress` の
//!   applied_count を更新し、途中失敗しても再実行時は**失敗した文から再開**する
//!   （先頭からのやり直しで `already exists` に詰まらない）。
//!   DSQL は DDL と DML を同一 tx に混ぜられないため「文の適用」と「進捗更新」は
//!   別 autocommit になり、ハードクラッシュが両者の境界に入った稀なケースのみ
//!   1 文が再実行されうる（その文を冪等に書くか手で 1 回触れば吸収できる）。
//! - 適用済み管理は `schema_migrations` テーブル（version = ファイル名）。全文成功時に
//!   初めて記録し、進捗行を消す。diff 用 baseline 再生では `record_history=false` で
//!   履歴・進捗テーブルを作らない
//! - 非 DSQL（ローカル PG）へは `CREATE INDEX ASYNC` を同期 `CREATE INDEX` に変換する

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

/// 1 つのマイグレーションファイル。
pub struct Migration {
    /// ファイル名（`schema_migrations.version` に記録される）
    pub version: String,
    pub sql: String,
}

/// 適用オプション。
pub struct ApplyOptions {
    /// DSQL への適用なら true。false ならローカル PG 向けに ASYNC index を同期に変換する
    pub is_dsql: bool,
    /// schema_migrations で適用済み管理を行うか（diff 用 baseline 再生では false）
    pub record_history: bool,
}

/// ディレクトリから *.sql をファイル名昇順で読み込む。
pub fn load_migrations(dir: &Path) -> anyhow::Result<Vec<Migration>> {
    let mut migrations = Vec::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("migrations ディレクトリを読めません: {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let version = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("ファイル名が UTF-8 ではありません")?
            .to_owned();
        let sql = std::fs::read_to_string(&path)
            .with_context(|| format!("読み込みに失敗: {}", path.display()))?;
        migrations.push(Migration { version, sql });
    }
    migrations.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(migrations)
}

/// SQL テキストをステートメント列に分割する（行末の `;` を文の終端とみなす。
/// 文字列リテラル内に行末 `;` を書かない規約）。コメントだけの塊は捨てる。
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut buf = String::new();
    let mut push = |chunk: &str| {
        let stmt = chunk.trim().trim_end_matches(';').trim();
        if !stmt.is_empty()
            && !stmt
                .lines()
                .all(|l| l.trim().is_empty() || l.trim().starts_with("--"))
        {
            statements.push(stmt.to_owned());
        }
    };
    for line in sql.lines() {
        buf.push_str(line);
        buf.push('\n');
        if line.trim_end().ends_with(';') {
            push(&buf);
            buf.clear();
        }
    }
    push(&buf);
    statements
}

/// DSQL 方言の文をローカル PG 用に変換する（ASYNC index → 同期 index）。
pub fn to_local_pg(stmt: &str) -> String {
    stmt.replace("CREATE UNIQUE INDEX ASYNC", "CREATE UNIQUE INDEX")
        .replace("CREATE INDEX ASYNC", "CREATE INDEX")
}

async fn execute(db: &DatabaseConnection, sql: &str) -> anyhow::Result<()> {
    db.execute_unprepared(sql)
        .await
        .with_context(|| format!("SQL 実行に失敗: {}", sql.lines().next().unwrap_or("")))?;
    Ok(())
}

/// SQL 文字列リテラルに埋め込む値をエスケープする（version = ファイル名。
/// `'` を含む名前で文が壊れないよう、埋め込み箇所は必ずここを通す）。
fn sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// version の進捗行を削除する。
async fn delete_progress(db: &DatabaseConnection, version: &str) -> anyhow::Result<()> {
    execute(
        db,
        &format!(
            "DELETE FROM schema_migration_progress WHERE version = '{}'",
            sql_literal(version)
        ),
    )
    .await
}

/// 進捗表に「version の適用済み文数」を設定する（DSQL は UPSERT 不可なので delete+insert）。
async fn set_progress(db: &DatabaseConnection, version: &str, count: usize) -> anyhow::Result<()> {
    delete_progress(db, version).await?;
    execute(
        db,
        &format!(
            "INSERT INTO schema_migration_progress (version, applied_count) VALUES ('{}', {count})",
            sql_literal(version)
        ),
    )
    .await?;
    Ok(())
}

/// 未適用のマイグレーションを順に適用し、適用した version を返す。
///
/// 途中失敗した migration は次回、`schema_migration_progress` に記録された文数の
/// 次の文から再開する（既に適用済みの文は skip）。全文成功して初めて
/// `schema_migrations` に記録し、進捗行を削除する。
pub async fn apply_migrations(
    db: &DatabaseConnection,
    migrations: &[Migration],
    opts: &ApplyOptions,
) -> anyhow::Result<Vec<String>> {
    let mut applied: HashSet<String> = HashSet::new();
    let mut progress: HashMap<String, usize> = HashMap::new();
    if opts.record_history {
        execute(
            db,
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
               version text NOT NULL PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
        )
        .await?;
        execute(
            db,
            "CREATE TABLE IF NOT EXISTS schema_migration_progress (\
               version text NOT NULL PRIMARY KEY, applied_count integer NOT NULL)",
        )
        .await?;
        applied = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT version FROM schema_migrations",
            ))
            .await?
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "version").ok())
            .collect();
        progress = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT version, applied_count FROM schema_migration_progress",
            ))
            .await?
            .into_iter()
            .filter_map(|row| {
                let version = row.try_get::<String>("", "version").ok()?;
                let count = row.try_get::<i32>("", "applied_count").ok()?;
                Some((version, count.max(0) as usize))
            })
            .collect();
    }

    let mut newly_applied = Vec::new();
    for migration in migrations {
        if applied.contains(&migration.version) {
            // 「履歴の記録」と「進捗行の削除」の境界にクラッシュが入ると進捗行だけが
            // 残り続ける。適用済みと分かった時点で掃除する
            if opts.record_history && progress.contains_key(&migration.version) {
                delete_progress(db, &migration.version).await?;
            }
            continue;
        }
        // 前回途中まで適用済みなら、その次の文から再開する
        let start = progress.get(&migration.version).copied().unwrap_or(0);
        for (i, stmt) in split_statements(&migration.sql).into_iter().enumerate() {
            if i < start {
                continue;
            }
            let stmt = if opts.is_dsql {
                stmt
            } else {
                to_local_pg(&stmt)
            };
            execute(db, &stmt).await?;
            if opts.record_history {
                set_progress(db, &migration.version, i + 1).await?;
            }
        }
        if opts.record_history {
            execute(
                db,
                &format!(
                    "INSERT INTO schema_migrations (version) VALUES ('{}')",
                    sql_literal(&migration.version)
                ),
            )
            .await?;
            delete_progress(db, &migration.version).await?;
        }
        newly_applied.push(migration.version.clone());
    }
    Ok(newly_applied)
}

/// schema.sql（素の PG 方言）をそのまま実行する（lab / scratch の再構築用）。
pub async fn execute_sql(db: &DatabaseConnection, sql: &str) -> anyhow::Result<()> {
    for stmt in split_statements(sql) {
        execute(db, &stmt).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_statements_は行末セミコロンで区切る() {
        let sql =
            "-- コメント\nCREATE TABLE a (\n  id uuid NOT NULL\n);\n\nCREATE INDEX i ON a (id);\n";
        let stmts = split_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("-- コメント\nCREATE TABLE a"));
        assert_eq!(stmts[1], "CREATE INDEX i ON a (id)");
    }

    #[test]
    fn split_statements_はコメントだけの塊を捨てる() {
        let stmts = split_statements("-- ヘッダ\n-- 説明;\n\nCREATE TABLE a (id uuid);\n-- 末尾\n");
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn to_local_pg_は_async_index_を同期に変換する() {
        assert_eq!(
            to_local_pg("CREATE UNIQUE INDEX ASYNC u ON t (c)"),
            "CREATE UNIQUE INDEX u ON t (c)"
        );
        assert_eq!(
            to_local_pg("CREATE INDEX ASYNC i ON t (c)"),
            "CREATE INDEX i ON t (c)"
        );
        assert_eq!(
            to_local_pg("CREATE TABLE t (id uuid)"),
            "CREATE TABLE t (id uuid)"
        );
    }

    #[test]
    fn sql_literal_はシングルクォートをエスケープする() {
        assert_eq!(sql_literal("a'b"), "a''b");
        assert_eq!(sql_literal("plain"), "plain");
    }

    /// 再開は「先頭から start 文を飛ばす」だけ、という位置ロジックを純粋に確認する。
    #[test]
    fn 再開位置は先頭から_start_文を飛ばす() {
        let sql = "CREATE TABLE a (id int);\nCREATE TABLE b (id int);\nCREATE TABLE c (id int);\n";
        let start = 1;
        let remaining: Vec<String> = split_statements(sql)
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i >= start)
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            remaining,
            vec![
                "CREATE TABLE b (id int)".to_string(),
                "CREATE TABLE c (id int)".to_string(),
            ]
        );
    }

    // --- DB を使う統合テスト（DSQL_SCHEMA_TEST_PG_URL の指す使い捨て PG に対して実行。
    //     未設定なら skip。schema_migrations / schema_migration_progress / cp_* を掃除する）---

    async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
        db.query_all(Statement::from_string(db.get_database_backend(), sql))
            .await
            .unwrap()
            .first()
            .and_then(|r| r.try_get::<i64>("", "c").ok())
            .unwrap_or(0)
    }

    async fn table_exists(db: &DatabaseConnection, name: &str) -> bool {
        count(
            db,
            &format!(
                "SELECT count(*) AS c FROM information_schema.tables \
                 WHERE table_schema='public' AND table_name='{name}'"
            ),
        )
        .await
            > 0
    }

    async fn applied_contains(db: &DatabaseConnection, version: &str) -> bool {
        count(
            db,
            &format!("SELECT count(*) AS c FROM schema_migrations WHERE version='{version}'"),
        )
        .await
            > 0
    }

    async fn progress_count(db: &DatabaseConnection, version: &str) -> Option<usize> {
        db.query_all(Statement::from_string(
            db.get_database_backend(),
            format!(
                "SELECT applied_count FROM schema_migration_progress WHERE version='{version}'"
            ),
        ))
        .await
        .unwrap()
        .first()
        .and_then(|r| r.try_get::<i32>("", "applied_count").ok())
        .map(|c| c.max(0) as usize)
    }

    async fn cleanup(db: &DatabaseConnection) {
        for t in [
            "cp_a",
            "cp_b",
            "cp_c",
            "schema_migrations",
            "schema_migration_progress",
        ] {
            execute(db, &format!("DROP TABLE IF EXISTS {t}"))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn 部分失敗した_migration_は失敗文から再開する() {
        let Ok(url) = std::env::var("DSQL_SCHEMA_TEST_PG_URL") else {
            eprintln!("DSQL_SCHEMA_TEST_PG_URL 未設定のため skip");
            return;
        };
        let db = crate::connect::connect(&crate::connect::DbSource::Direct(url))
            .await
            .unwrap();
        cleanup(&db).await;
        let opts = ApplyOptions {
            is_dsql: false,
            record_history: true,
        };

        // 1 回目: 2 文目が不正 SQL で失敗する
        let broken = [Migration {
            version: "v1".to_string(),
            sql: "CREATE TABLE cp_a (id int);\nTHIS IS NOT SQL;\nCREATE TABLE cp_c (id int);\n"
                .to_string(),
        }];
        assert!(
            apply_migrations(&db, &broken, &opts).await.is_err(),
            "2 文目で失敗するはず"
        );
        // cp_a は適用済み・cp_c は未適用・v1 は未記録・進捗は 1
        assert!(table_exists(&db, "cp_a").await);
        assert!(!table_exists(&db, "cp_c").await);
        assert!(!applied_contains(&db, "v1").await);
        assert_eq!(progress_count(&db, "v1").await, Some(1));

        // 2 回目: 2 文目を修正して再実行 → cp_a は skip され cp_b / cp_c を適用
        let fixed = [Migration {
            version: "v1".to_string(),
            sql: "CREATE TABLE cp_a (id int);\nCREATE TABLE cp_b (id int);\nCREATE TABLE cp_c (id int);\n"
                .to_string(),
        }];
        let done = apply_migrations(&db, &fixed, &opts).await.unwrap();
        assert_eq!(done, vec!["v1".to_string()]);
        assert!(table_exists(&db, "cp_b").await);
        assert!(table_exists(&db, "cp_c").await);
        assert!(applied_contains(&db, "v1").await);
        assert_eq!(progress_count(&db, "v1").await, None); // 完了で進捗行は削除

        // 3 回目: 既適用なので何もしない（冪等）
        let again = apply_migrations(&db, &fixed, &opts).await.unwrap();
        assert!(again.is_empty());

        // 4 回目: 「履歴の記録 → 進捗行の削除」の境界クラッシュで残った stale な
        // 進捗行は、次の実行で掃除される
        execute(
            &db,
            "INSERT INTO schema_migration_progress (version, applied_count) VALUES ('v1', 3)",
        )
        .await
        .unwrap();
        let again = apply_migrations(&db, &fixed, &opts).await.unwrap();
        assert!(again.is_empty());
        assert_eq!(progress_count(&db, "v1").await, None);

        cleanup(&db).await;
    }
}
