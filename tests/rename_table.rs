//! `rename-table` の結合テスト（ローカル PG + psqldef が要る）。
//!
//! 環境変数 `DSQL_SCHEMA_TEST_PG_HOST`（例: `127.0.0.1`）が設定されているときだけ走り、
//! 無ければ skip する（`tests/diff_drop_constraint.rs` と同じ規約。CI では PostgreSQL
//! service を立てて必ず走らせる）。
//!
//! 確認すること（受け入れ条件）:
//! - 生成した migration を migrations/ に置き、schema.sql のテーブル・制約・index の名前を
//!   新名に直して `diff` を回すと差分なし（migration が増えない）
//! - 生成文の順序はテーブル → PK / UNIQUE 制約 → index。prefix 規則に合わない index と
//!   CHECK 制約は変えず、先頭コメントに注記する
//! - baseline に無いテーブル、名前の衝突は止まり、migration ファイルを残さない

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use dsql_schema_migrate::connect::{self, DbSource};
use sea_orm::ConnectionTrait;

/// 制約 3 種 + 単独 index 3 本（prefix 規則外 1 本）。psqldef export の正規形で書く
const INIT: &str = r#"CREATE TABLE "public"."items" (
    "code" text NOT NULL,
    "id" uuid NOT NULL,
    "slug" text NOT NULL,
    "status" text NOT NULL,
    CONSTRAINT "items_code_key" UNIQUE ("code"),
    CONSTRAINT "items_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "items_status_check" CHECK (status IN ('inbox', 'todo'))
);

CREATE INDEX "idx_items_created" ON "public"."items" ("status", "code");

CREATE UNIQUE INDEX "items_slug_key" ON "public"."items" ("slug");

CREATE INDEX "items_status_idx" ON "public"."items" ("status");
"#;

/// rename 後の schema.sql（テーブル名・prefix 規則に合う制約 / index 名を things に。
/// CHECK と規則外 index は旧名のまま）
const RENAMED: &str = r#"CREATE TABLE "public"."things" (
    "code" text NOT NULL,
    "id" uuid NOT NULL,
    "slug" text NOT NULL,
    "status" text NOT NULL,
    CONSTRAINT "things_code_key" UNIQUE ("code"),
    CONSTRAINT "things_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "items_status_check" CHECK (status IN ('inbox', 'todo'))
);

CREATE INDEX "idx_items_created" ON "public"."things" ("status", "code");

CREATE UNIQUE INDEX "things_slug_key" ON "public"."things" ("slug");

CREATE INDEX "things_status_idx" ON "public"."things" ("status");
"#;

const EXPECTED: [&str; 5] = [
    "ALTER TABLE \"public\".\"items\" RENAME TO \"things\";",
    "ALTER TABLE \"public\".\"things\" RENAME CONSTRAINT \"items_code_key\" TO \"things_code_key\";",
    "ALTER TABLE \"public\".\"things\" RENAME CONSTRAINT \"items_pkey\" TO \"things_pkey\";",
    "ALTER INDEX \"public\".\"items_slug_key\" RENAME TO \"things_slug_key\";",
    "ALTER INDEX \"public\".\"items_status_idx\" RENAME TO \"things_status_idx\";",
];

struct Env {
    host: String,
    user: String,
    project: PathBuf,
    scratch_db: String,
}

impl Env {
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
        let project = std::env::temp_dir().join(format!("dsql-schema-migrate-rename-{run_id}"));
        fs::create_dir_all(project.join("migrations")).unwrap();
        fs::write(project.join("migrations/20260101000000_init.sql"), INIT).unwrap();
        Some(Self {
            host,
            user,
            project,
            scratch_db: format!("_dsql_scratch_rename_{run_id}"),
        })
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_dsql-schema-migrate"))
            .current_dir(&self.project)
            .args(args)
            .args(["--pg-host", &self.host, "--pg-user", &self.user])
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

    async fn drop_scratch(&self) {
        let admin = connect::connect(&DbSource::Direct(connect::local_url(
            &self.user, &self.host, 5432, "postgres",
        )))
        .await
        .expect("ローカル PG に接続できない");
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

#[test]
fn rename_table_の出力を置いて_schema_sql_を直すと_diff_は差分なし() {
    let Some(env) = Env::detect() else {
        eprintln!(
            "skip: DSQL_SCHEMA_TEST_PG_HOST が未設定 (ローカル PG + psqldef が要る結合テスト)"
        );
        return;
    };
    let baseline = env.migrations();

    // baseline に無いテーブルは止まり、ファイルを残さない
    let (ok, _, stderr) = env.run(&["rename-table", "nope", "things"]);
    assert!(!ok, "存在しないテーブルは止まるべき");
    assert!(
        stderr.contains("baseline") && stderr.contains("public.nope"),
        "メッセージが違う: {stderr}"
    );
    assert_eq!(env.migrations(), baseline);

    // 生成: 文の順序・注記・ファイル名
    let (ok, stdout, stderr) = env.run(&["rename-table", "items", "things"]);
    assert!(ok, "rename-table は通るべき: {stderr}");
    let generated: Vec<String> = env
        .migrations()
        .into_iter()
        .filter(|n| !baseline.contains(n))
        .collect();
    assert_eq!(generated.len(), 1, "migration が 1 件増える: {generated:?}");
    assert!(
        generated[0].ends_with("_rename_items_to_things.sql"),
        "ファイル名が違う: {}",
        generated[0]
    );
    let content = fs::read_to_string(env.project.join("migrations").join(&generated[0])).unwrap();
    let body: Vec<&str> = content
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .collect();
    assert_eq!(body, EXPECTED, "生成文が違う: {content}");
    assert!(
        content.contains("-- - CHECK 制約 items_status_check"),
        "CHECK の注記が無い: {content}"
    );
    assert!(
        content.contains("-- - index idx_items_created"),
        "規則外 index の注記が無い: {content}"
    );
    assert!(stdout.contains("次: schema.sql"), "案内が無い: {stdout}");

    // 受け入れ条件: schema.sql を新名に直した diff が差分なし
    fs::write(env.project.join("schema.sql"), RENAMED).unwrap();
    let after = env.migrations();
    let (ok, stdout, stderr) = env.run(&["diff", "after_rename"]);
    assert!(ok, "diff は通るべき: {stderr}");
    assert!(stdout.contains("差分なし"), "差分が残っている: {stdout}");
    assert_eq!(
        env.migrations(),
        after,
        "diff が migration を作ってはいけない"
    );

    // 名前の衝突は止まり、ファイルを残さない (rename 後に別テーブルの index が items_status_idx を
    // 取っている状態を、rename migration の後ろに並ぶファイル名で作る)
    fs::write(
        env.project.join("migrations/20991231000000_other.sql"),
        "CREATE TABLE \"public\".\"other\" (\"n\" integer NOT NULL);\n\n\
         CREATE INDEX \"items_status_idx\" ON \"public\".\"other\" (\"n\");\n",
    )
    .unwrap();
    let before = env.migrations();
    let (ok, _, stderr) = env.run(&["rename-table", "things", "items"]);
    assert!(!ok, "衝突は止まるべき");
    assert!(
        stderr.contains("既にあります") && stderr.contains("items_status_idx"),
        "衝突のメッセージが違う: {stderr}"
    );
    assert_eq!(env.migrations(), before);

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(env.drop_scratch());
}
