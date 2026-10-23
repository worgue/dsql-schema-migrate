//! `diff` の DROP CONSTRAINT 検証の結合テスト（ローカル PG + psqldef が要る）。
//!
//! 環境変数 `DSQL_SCHEMA_TEST_PG_HOST`（例: `127.0.0.1`）が設定されているときだけ走り、
//! 無ければ skip する（unit test はローカル PG なしで動く、という既定を崩さない。
//! CI では PostgreSQL service を立てて必ず走らせる）。PG ユーザーは
//! `DSQL_SCHEMA_TEST_PG_USER`（既定 `postgres`）、ポートは 5432。psqldef は PATH から探す。
//!
//! 確認すること:
//! - CHECK / UNIQUE の drop は migration が生成され、その文を baseline に流すと制約が消える
//! - PRIMARY KEY の drop と ADD CONSTRAINT は種別を含むメッセージで止まり、
//!   migration ファイルが作られない
//! - `dsqlize_with_baseline` を実 PG に直接かけて、baseline に無い制約は Err、
//!   引用識別子（大文字混じり）のテーブル名は catalog で解決できる

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use dsql_schema_migrate::connect::{self, DbSource};
use dsql_schema_migrate::dsqlize;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

const INIT: &str = r#"CREATE TABLE "public"."items" (
    "id" uuid NOT NULL,
    "status" text NOT NULL,
    "code" text NOT NULL,
    CONSTRAINT "items_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "items_status_check" CHECK (status IN ('inbox', 'todo')),
    CONSTRAINT "items_code_key" UNIQUE ("code")
);
"#;

const DROP_CHECK: &str = "ALTER TABLE \"public\".\"items\" DROP CONSTRAINT \"items_status_check\"";
const DROP_UNIQUE: &str = "ALTER TABLE \"public\".\"items\" DROP CONSTRAINT \"items_code_key\"";

struct Env {
    host: String,
    user: String,
    project: PathBuf,
    /// 実行ごとに固有（同じ PG に対する並行実行で baseline の再生が競合しないように）
    scratch_db: String,
}

impl Env {
    /// 使えなければ None（テストは skip）。
    fn detect() -> Option<Self> {
        let host = std::env::var("DSQL_SCHEMA_TEST_PG_HOST")
            .ok()
            .filter(|v| !v.is_empty())?;
        let user = std::env::var("DSQL_SCHEMA_TEST_PG_USER").unwrap_or_else(|_| "postgres".into());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let run_id = format!("{}_{nanos}", std::process::id());
        let project = std::env::temp_dir().join(format!("dsql-schema-migrate-test-{run_id}"));
        fs::create_dir_all(project.join("migrations")).unwrap();
        fs::write(project.join("migrations/20260101000000_init.sql"), INIT).unwrap();
        Some(Self {
            host,
            user,
            project,
            scratch_db: format!("_dsql_scratch_test_{run_id}"),
        })
    }

    fn diff(&self, name: &str, schema: &str) -> (bool, String, String) {
        fs::write(self.project.join("schema.sql"), schema).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_dsql-schema-migrate"))
            .current_dir(&self.project)
            .args([
                "diff",
                name,
                "--pg-host",
                &self.host,
                "--pg-user",
                &self.user,
            ])
            .args(["--scratch-db", &self.scratch_db])
            .output()
            .expect("CLI を起動できない");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn migrations(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.project.join("migrations"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    async fn connect(&self, dbname: &str) -> DatabaseConnection {
        connect::connect(&DbSource::Direct(connect::local_url(
            &self.user, &self.host, 5432, dbname,
        )))
        .await
        .expect("ローカル PG に接続できない")
    }

    async fn constraint_names(&self, db: &DatabaseConnection, table: &str) -> Vec<String> {
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT c.conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
                 WHERE t.relname = $1 ORDER BY c.conname",
                [table.into()],
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| r.try_get::<String>("", "conname").unwrap())
            .collect()
    }

    /// scratch DB を落とす（diff が作った baseline の片付け）。
    async fn drop_scratch(&self) {
        let admin = self.connect("postgres").await;
        admin
            .execute_unprepared(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.scratch_db
            ))
            .await
            .unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.project);
    }
}

/// INIT から `line_contains` を含む行（= 制約 1 つ）を外した schema.sql を作る。
/// 外した行が最後の要素だった場合に残る末尾カンマは取り除く。
fn without(line_contains: &str) -> String {
    INIT.lines()
        .filter(|l| !l.contains(line_contains))
        .map(|l| format!("{l}\n"))
        .collect::<String>()
        .replace(",\n);", "\n);")
}

#[test]
fn diff_は_check_と_unique_の_drop_constraint_だけ機械変換する() {
    let Some(env) = Env::detect() else {
        eprintln!(
            "skip: DSQL_SCHEMA_TEST_PG_HOST が未設定 (ローカル PG + psqldef が要る結合テスト)"
        );
        return;
    };
    let baseline = env.migrations();

    // CHECK / UNIQUE を外す → その DROP CONSTRAINT だけの migration が生成される
    for (name, line, stmt) in [
        ("drop_check", "items_status_check", DROP_CHECK),
        ("drop_unique", "items_code_key", DROP_UNIQUE),
    ] {
        let (ok, stdout, stderr) = env.diff(name, &without(line));
        assert!(ok, "{name} は通るべき: {stderr}");
        assert!(stdout.contains(stmt), "生成文が違う: {stdout}");
        let generated: Vec<String> = env
            .migrations()
            .into_iter()
            .filter(|n| !baseline.contains(n))
            .collect();
        assert_eq!(generated.len(), 1, "migration が 1 件増える: {generated:?}");
        let path = env.project.join("migrations").join(&generated[0]);
        let content = fs::read_to_string(&path).unwrap();
        let body: Vec<&str> = content
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with("--"))
            .collect();
        assert_eq!(body, [format!("{stmt};")], "ファイル内容が違う: {content}");
        fs::remove_file(&path).unwrap();
    }

    // PRIMARY KEY の drop は DSQL の実測エラー文を含むメッセージで止まり、ファイルを残さない
    let (ok, _, stderr) = env.diff("drop_pk", &without("items_pkey"));
    assert!(!ok, "PRIMARY KEY の drop は止まるべき");
    assert!(
        stderr.contains("PRIMARY KEY") && stderr.contains("items_pkey"),
        "種別・制約名がメッセージに無い: {stderr}"
    );
    assert!(
        stderr.contains("unsupported ALTER TABLE DROP CONSTRAINT"),
        "DSQL の実測エラー文が無い: {stderr}"
    );
    assert_eq!(
        env.migrations(),
        baseline,
        "止まったのに migration が残っている"
    );

    // ADD CONSTRAINT は従来どおり fail-loud
    let added = INIT.replace(
        "    CONSTRAINT \"items_code_key\" UNIQUE (\"code\")\n",
        "    CONSTRAINT \"items_code_key\" UNIQUE (\"code\"),\n    CONSTRAINT \"items_code_check\" CHECK (code <> '')\n",
    );
    let (ok, _, stderr) = env.diff("add_check", &added);
    assert!(!ok, "ADD CONSTRAINT は止まるべき");
    assert!(
        stderr.contains("機械変換できない"),
        "メッセージが違う: {stderr}"
    );
    assert_eq!(env.migrations(), baseline);

    // ここから lib API を実 PG (直前の diff が再生した baseline) に直接かける
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = env.connect(&env.scratch_db).await;

        // baseline に無い制約名は Err (migrations と schema.sql の不整合の検出)
        let err = dsqlize::dsqlize_with_baseline(
            &db,
            &["ALTER TABLE \"public\".\"items\" DROP CONSTRAINT \"nope\"".to_owned()],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("baseline"), "不在のメッセージが違う: {err}");
        assert!(err.contains("nope"), "制約名が無い: {err}");

        // 引用識別子 (大文字・空白混じり) のテーブル名と制約名を catalog で解決できる
        db.execute_unprepared(
            "CREATE TABLE \"public\".\"Mixed Case\" (\"n\" integer, \
             CONSTRAINT \"Mixed Check\" CHECK (n > 0))",
        )
        .await
        .unwrap();
        let mixed = "ALTER TABLE \"public\".\"Mixed Case\" DROP CONSTRAINT \"Mixed Check\"";
        let out = dsqlize::dsqlize_with_baseline(&db, &[mixed.to_owned()])
            .await
            .expect("引用識別子の CHECK は通るべき");
        assert_eq!(out, [mixed]);

        // 生成した文を baseline に流すと、対象の制約だけが消える
        assert_eq!(
            env.constraint_names(&db, "items").await,
            ["items_code_key", "items_pkey", "items_status_check"]
        );
        db.execute_unprepared(DROP_CHECK).await.unwrap();
        db.execute_unprepared(DROP_UNIQUE).await.unwrap();
        assert_eq!(env.constraint_names(&db, "items").await, ["items_pkey"]);
        drop(db);
        env.drop_scratch().await;
    });
}
