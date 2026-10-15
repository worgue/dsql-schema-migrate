//! `diff` の DROP CONSTRAINT 検証の結合テスト（ローカル PG + psqldef が要る）。
//!
//! 環境変数 `DSQL_SCHEMA_TEST_PG_HOST`（例: `127.0.0.1`）が設定されているときだけ走り、
//! 無ければ skip する（unit test はローカル PG なしで動く、という既定を崩さない）。
//! PG ユーザーは `DSQL_SCHEMA_TEST_PG_USER`（既定 `postgres`）。psqldef は PATH から探す。
//!
//! 確認すること: CHECK の drop は migration が生成される / PRIMARY KEY・UNIQUE の drop と
//! ADD CONSTRAINT は種別を含むメッセージで止まる / 止まったときに migration ファイルが
//! 作られない。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const INIT: &str = r#"CREATE TABLE "public"."items" (
    "id" uuid NOT NULL,
    "status" text NOT NULL,
    "code" text NOT NULL,
    CONSTRAINT "items_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "items_status_check" CHECK (status IN ('inbox', 'todo')),
    CONSTRAINT "items_code_key" UNIQUE ("code")
);
"#;

struct Env {
    host: String,
    user: String,
    project: PathBuf,
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
        let project = std::env::temp_dir().join(format!(
            "dsql-schema-migrate-test-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(project.join("migrations")).unwrap();
        fs::write(project.join("migrations/20260101000000_init.sql"), INIT).unwrap();
        Some(Self {
            host,
            user,
            project,
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
            .args(["--scratch-db", "_dsql_scratch_test_drop_constraint"])
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
fn diff_は_check_の_drop_constraint_だけ機械変換する() {
    let Some(env) = Env::detect() else {
        eprintln!(
            "skip: DSQL_SCHEMA_TEST_PG_HOST が未設定 (ローカル PG + psqldef が要る結合テスト)"
        );
        return;
    };
    let baseline = env.migrations();

    // CHECK を外す → DROP CONSTRAINT が生成される
    let (ok, stdout, stderr) = env.diff("drop_check", &without("items_status_check"));
    assert!(ok, "CHECK の drop は通るべき: {stderr}");
    assert!(
        stdout.contains("ALTER TABLE \"public\".\"items\" DROP CONSTRAINT \"items_status_check\";"),
        "生成文が違う: {stdout}"
    );
    let generated = env.migrations();
    assert_eq!(
        generated.len(),
        baseline.len() + 1,
        "migration が 1 件増える"
    );
    for name in &generated {
        if !baseline.contains(name) {
            fs::remove_file(env.project.join("migrations").join(name)).unwrap();
        }
    }

    // UNIQUE / PRIMARY KEY の drop は種別を含むメッセージで止まり、ファイルを残さない
    for (name, line, kind) in [
        ("drop_unique", "items_code_key", "UNIQUE"),
        ("drop_pk", "items_pkey", "PRIMARY KEY"),
    ] {
        let (ok, _, stderr) = env.diff(name, &without(line));
        assert!(!ok, "{kind} の drop は止まるべき");
        assert!(
            stderr.contains(kind),
            "種別 {kind} がメッセージに無い: {stderr}"
        );
        assert!(
            stderr.contains(line),
            "制約名 {line} がメッセージに無い: {stderr}"
        );
        assert_eq!(
            env.migrations(),
            baseline,
            "止まったのに migration が残っている"
        );
    }

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
}
