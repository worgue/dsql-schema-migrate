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
//! - **OCC 競合の文単位再試行**: DSQL は楽観的同時実行制御なので、`CREATE INDEX ASYNC` の
//!   ジョブが schema を更新している最中に次の DDL を出すと `OC001`（schema has been updated
//!   by another transaction）で落ちる（新しいクラスターに多数の DDL を続けて流す初回
//!   bootstrap で起きやすい）。1 文 1 autocommit なので失敗した文は丸ごと巻き戻っており、
//!   その文だけを指数バックオフで再試行する（[`OCC_SQLSTATES`] / [`OCC_MAX_RETRIES`]）。
//!   再試行で解消しなければエラーに「再実行すると失敗した文から再開する」旨を含めて止まる

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, RuntimeErr, Statement};

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

/// OCC 競合として文単位で再試行する SQLSTATE。
///
/// - `OC000`: 他トランザクションとの変更競合（DSQL 固有）
/// - `OC001`: schema が他トランザクションに更新された（DSQL 固有。ASYNC index のジョブが
///   schema を更新している最中に次の DDL を出すと起きる）
/// - `40001`: serialization_failure（PostgreSQL 標準。DSQL も同じ意味で返しうる）
pub const OCC_SQLSTATES: &[&str] = &["OC000", "OC001", "40001"];

/// OCC 競合で同じ文を再試行する回数の上限（初回実行を除く）。
pub const OCC_MAX_RETRIES: u32 = 6;

/// `retry` 回目（0 始まり）の再試行前に待つ時間。250ms から倍々で 4s を上限とする
/// （[`OCC_MAX_RETRIES`] 回で合計約 12 秒。ASYNC index ジョブの schema 更新は数秒で収まる想定）。
pub fn occ_backoff(retry: u32) -> Duration {
    let ms = 250u64 << retry.min(4);
    Duration::from_millis(ms.min(4000))
}

/// `sqlstate` が OCC 競合（再試行で解消しうるエラー）かを返す。
pub fn is_occ_conflict(sqlstate: &str) -> bool {
    OCC_SQLSTATES.contains(&sqlstate)
}

/// [`DbErr`] から SQLSTATE を取り出す（driver 由来の DB エラーでなければ `None`）。
pub fn sqlstate_of(err: &DbErr) -> Option<String> {
    match err {
        DbErr::Exec(RuntimeErr::SqlxError(e)) | DbErr::Query(RuntimeErr::SqlxError(e)) => {
            match &**e {
                sea_orm::sqlx::Error::Database(db_err) => db_err.code().map(|c| c.into_owned()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 1 文を autocommit で実行する。OCC 競合（[`OCC_SQLSTATES`]）なら `max_retries` 回まで
/// [`occ_backoff`] で待って同じ文をやり直す。それ以外のエラーは即座に返す。
async fn execute_with_retries(
    db: &DatabaseConnection,
    sql: &str,
    max_retries: u32,
) -> anyhow::Result<()> {
    let head = sql.lines().next().unwrap_or("");
    let mut retry = 0;
    loop {
        let err = match db.execute_unprepared(sql).await {
            Ok(_) => return Ok(()),
            Err(err) => err,
        };
        let occ = sqlstate_of(&err).filter(|code| is_occ_conflict(code));
        match occ {
            Some(code) if retry < max_retries => {
                let wait = occ_backoff(retry);
                retry += 1;
                eprintln!(
                    "OCC 競合 ({code}) のため {}ms 待って再試行 {retry}/{max_retries}: {head}",
                    wait.as_millis()
                );
                tokio::time::sleep(wait).await;
            }
            Some(code) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "SQL 実行に失敗 (OCC 競合 {code} が {max_retries} 回の再試行でも解消しませんでした。\
                     少し待って再実行すると失敗した文から再開します): {head}"
                )));
            }
            None => {
                return Err(anyhow::Error::new(err).context(format!("SQL 実行に失敗: {head}")));
            }
        }
    }
}

async fn execute(db: &DatabaseConnection, sql: &str) -> anyhow::Result<()> {
    execute_with_retries(db, sql, OCC_MAX_RETRIES).await
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
            .query_all_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT version FROM schema_migrations",
            ))
            .await?
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "version").ok())
            .collect();
        progress = db
            .query_all_raw(Statement::from_string(
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

    #[test]
    fn is_occ_conflict_は_dsql_の_occ_系と_40001_だけ真() {
        assert!(is_occ_conflict("OC000"));
        assert!(is_occ_conflict("OC001"));
        assert!(is_occ_conflict("40001"));
        assert!(!is_occ_conflict("42P01")); // undefined_table
        assert!(!is_occ_conflict("42P07")); // duplicate_table
        assert!(!is_occ_conflict(""));
    }

    #[test]
    fn occ_backoff_は倍々で_4秒を上限にする() {
        assert_eq!(occ_backoff(0), Duration::from_millis(250));
        assert_eq!(occ_backoff(1), Duration::from_millis(500));
        assert_eq!(occ_backoff(3), Duration::from_millis(2000));
        assert_eq!(occ_backoff(4), Duration::from_millis(4000));
        assert_eq!(occ_backoff(5), Duration::from_millis(4000));
        assert_eq!(occ_backoff(100), Duration::from_millis(4000));
    }

    #[test]
    fn sqlstate_of_は_driver_由来でないエラーには_none() {
        assert_eq!(sqlstate_of(&DbErr::Custom("x".into())), None);
        assert_eq!(sqlstate_of(&DbErr::RecordNotFound("x".into())), None);
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

    // --- DB を使う統合テスト（使い捨てのローカル PG に対して実行。接続先は
    //     DSQL_SCHEMA_TEST_PG_URL、無ければ DSQL_SCHEMA_TEST_PG_HOST (+ _USER。tests/ と同じ
    //     規約) から組む。どちらも未設定なら skip。
    //     schema_migrations / schema_migration_progress / cp_* を掃除する）---

    fn test_pg_url() -> Option<String> {
        if let Ok(url) = std::env::var("DSQL_SCHEMA_TEST_PG_URL") {
            return Some(url);
        }
        let host = std::env::var("DSQL_SCHEMA_TEST_PG_HOST").ok()?;
        let user = std::env::var("DSQL_SCHEMA_TEST_PG_USER").unwrap_or_else(|_| "postgres".into());
        Some(crate::connect::local_url(&user, &host, 5432, "postgres"))
    }

    async fn test_db() -> Option<DatabaseConnection> {
        let Some(url) = test_pg_url() else {
            eprintln!("DSQL_SCHEMA_TEST_PG_URL / DSQL_SCHEMA_TEST_PG_HOST 未設定のため skip");
            return None;
        };
        Some(
            crate::connect::connect(&crate::connect::DbSource::Direct(url))
                .await
                .unwrap(),
        )
    }

    async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
        db.query_all_raw(Statement::from_string(db.get_database_backend(), sql))
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
        db.query_all_raw(Statement::from_string(
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
        let Some(db) = test_db().await else {
            return;
        };
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

    /// OCC 競合を模した文（SQLSTATE OC001 を RAISE する DO ブロック）が、再試行の途中で
    /// 通るようになれば成功し、上限まで通らなければ再開の案内付きで失敗することを確認する。
    /// 「何回目で通るか」は sequence（非トランザクショナルなので RAISE で巻き戻らない）で数える。
    #[tokio::test]
    async fn occ_競合は文単位で再試行し_上限で再開の案内付きで止まる() {
        let Some(db) = test_db().await else {
            return;
        };
        execute(&db, "DROP SEQUENCE IF EXISTS cp_occ_seq")
            .await
            .unwrap();
        execute(&db, "CREATE SEQUENCE cp_occ_seq").await.unwrap();
        // 1〜2 回目は OC001、3 回目で通る (再試行 2 回で解消)
        let flaky = "DO $$ BEGIN IF nextval('cp_occ_seq') < 3 THEN \
                     RAISE EXCEPTION 'schema has been updated by another transaction' \
                     USING ERRCODE = 'OC001'; END IF; END $$";
        execute_with_retries(&db, flaky, OCC_MAX_RETRIES)
            .await
            .expect("2 回の再試行で通るはず");
        assert_eq!(
            count(&db, "SELECT last_value AS c FROM cp_occ_seq").await,
            3
        );

        // 常に OC001 なら上限で止まり、エラーに再開の案内が入る
        let always = "DO $$ BEGIN RAISE EXCEPTION 'conflict' USING ERRCODE = 'OC001'; END $$";
        let err = execute_with_retries(&db, always, 1).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("OCC 競合 OC001"), "{msg}");
        assert!(msg.contains("失敗した文から再開"), "{msg}");

        // OCC 以外のエラーは再試行せず即座に返す (sequence は進まない)
        execute(&db, "SELECT setval('cp_occ_seq', 1)")
            .await
            .unwrap();
        let other = "DO $$ BEGIN PERFORM nextval('cp_occ_seq'); \
                     RAISE EXCEPTION 'nope' USING ERRCODE = '42P01'; END $$";
        let err = execute_with_retries(&db, other, OCC_MAX_RETRIES)
            .await
            .unwrap_err();
        assert!(!format!("{err:#}").contains("OCC 競合"));
        assert_eq!(
            count(&db, "SELECT last_value AS c FROM cp_occ_seq").await,
            2
        );

        // SQLSTATE の取り出しそのものも実 driver で確認する
        let raw = db
            .execute_unprepared("SELECT * FROM cp_no_such_table")
            .await
            .unwrap_err();
        assert_eq!(sqlstate_of(&raw).as_deref(), Some("42P01"));

        execute(&db, "DROP SEQUENCE IF EXISTS cp_occ_seq")
            .await
            .unwrap();
    }
}
