//! dsql-schema-migrate CLI。サブコマンドと入出力:
//!
//! | コマンド | 入力 | 出力 |
//! |---|---|---|
//! | `diff <name>` | schema.sql + migrations/（baseline 再生用）+ ローカル PG | `migrations/<ts>_<name>.sql`（DSQL 方言） |
//! | `apply` | migrations/ + 接続先 env（DATABASE_URL / DSQL_HOST） | なし（stdout に適用ログ、DB の schema_migrations 更新） |
//! | `rebuild-lab` | schema.sql + ローカル PG | lab DB（entity 生成用） |
//! | `export` | 接続先（lab または --dsql） | 正規形スキーマ（stdout / --write。--check でドリフト検査） |
//! | `status` | 接続先 env | stdout（schema_migrations / sys.jobs） |

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use sea_orm::{ConnectionTrait, Statement};

use crate::applier::{self, ApplyOptions};
use crate::connect::{self, DbSource};
use crate::{dsqlize, psqldef};

#[derive(Parser)]
#[command(
    name = "dsql-schema-migrate",
    about = "Aurora DSQL 向けスキーマ管理 (psqldef diff + 自前 applier)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// ローカル PostgreSQL（作業台）の接続情報。
#[derive(Args)]
struct LocalPg {
    /// ローカル PG のユーザー（既定 `postgres`。env: `DSQL_SCHEMA_PG_USER`）
    ///
    /// env は crate 固有の名前にすること。汎用の `USER` を読むと、ログイン名
    /// （VM では `forge` 等）が PG ユーザーとして採用されてしまい、既定値が
    /// 事実上機能しない（消費側が全レシピで `--pg-user postgres` を渡す羽目になる）。
    #[arg(long, env = "DSQL_SCHEMA_PG_USER", default_value = "postgres")]
    pg_user: String,
    #[arg(long, default_value = "127.0.0.1")]
    pg_host: String,
    #[arg(long, default_value_t = 5432)]
    pg_port: u16,
}

impl LocalPg {
    fn url(&self, dbname: &str) -> String {
        connect::local_url(&self.pg_user, &self.pg_host, self.pg_port, dbname)
    }

    fn psqldef_target(&self, dbname: &str) -> psqldef::Target {
        psqldef::Target {
            host: self.pg_host.clone(),
            port: self.pg_port,
            user: self.pg_user.clone(),
            dbname: dbname.to_owned(),
            password: None,
            ssl: false,
        }
    }

    async fn recreate(&self, dbname: &str) -> anyhow::Result<()> {
        connect::recreate_local_db(&self.pg_user, &self.pg_host, self.pg_port, dbname).await
    }
}

#[derive(Subcommand)]
enum Command {
    /// schema.sql と「migrations/ の再生結果」の差分から DSQL 方言の migration を生成する
    Diff {
        /// migration 名（ファイル名 `<UTC時刻>_<name>.sql` に使われる）
        name: String,
        /// 信頼の源となるスキーマファイル
        #[arg(long, default_value = "schema.sql")]
        schema: PathBuf,
        #[arg(long, default_value = "migrations")]
        migrations: PathBuf,
        /// baseline 再生に使う使い捨てローカル DB
        #[arg(long, default_value = "_dsql_scratch")]
        scratch_db: String,
        /// 破壊的変更（DROP TABLE 等）を diff に含める（既定は検出したらエラー）
        #[arg(long)]
        enable_drop: bool,
        #[command(flatten)]
        pg: LocalPg,
    },
    /// 未適用の migration を 1 文ずつ適用する（env: DATABASE_URL または DSQL_HOST）
    Apply {
        #[arg(long, default_value = "migrations")]
        migrations: PathBuf,
    },
    /// schema.sql から entity 生成用のローカル lab DB を再構築する
    RebuildLab {
        #[arg(long, default_value = "schema.sql")]
        schema: PathBuf,
        #[arg(long, default_value = "_dsql_lab")]
        lab_db: String,
        #[command(flatten)]
        pg: LocalPg,
    },
    /// 現在のスキーマを正規形で出力する（--check でドリフト検査）
    Export {
        /// 対象のローカル DB（--dsql 指定時は無視）
        #[arg(long, default_value = "_dsql_lab")]
        db: String,
        /// env の DSQL_HOST に接続して export する
        #[arg(long)]
        dsql: bool,
        /// schema.sql と比較し、乖離があれば exit 2（内容は <schema>.drift.sql に保存）
        #[arg(long)]
        check: bool,
        /// 出力先ファイル（省略時は stdout）
        #[arg(long)]
        write: Option<PathBuf>,
        /// --check の比較対象
        #[arg(long, default_value = "schema.sql")]
        schema: PathBuf,
        #[command(flatten)]
        pg: LocalPg,
    },
    /// 適用状況を表示する（env: DATABASE_URL または DSQL_HOST）
    Status,
}

/// `export --check` がドリフトを検出したことを表すエラー。
///
/// lib のコードからプロセスを殺さないために、exit code への変換は
/// エントリポイント（[`crate::run_cli`]）だけが行う。CI がドリフトと他の失敗を
/// 区別できるよう、この型に限り exit 2 を割り当てている（他のエラーは 1）。
#[derive(Debug)]
pub struct DriftDetected {
    /// 比較対象のスキーマファイル
    pub schema: PathBuf,
    /// 実スキーマの export 内容を保存したファイル
    pub drift_path: PathBuf,
}

/// [`DriftDetected`] に対応する終了コード。
pub const DRIFT_EXIT_CODE: i32 = 2;

impl std::fmt::Display for DriftDetected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ドリフト検出: 実スキーマが {} と一致しません。export を {} に保存しました (diff で比較を)",
            self.schema.display(),
            self.drift_path.display()
        )
    }
}

impl std::error::Error for DriftDetected {}

/// CLI のエントリポイント（引数はプロセスの argv から読む）。
///
/// 消費プロジェクトはこれを呼ぶ薄い bin を置くだけでよい:
///
/// ```ignore
/// fn main() -> anyhow::Result<()> {
///     dsql_schema_migrate::run_cli()
/// }
/// ```
pub async fn run() -> anyhow::Result<()> {
    // カレントの .env を読む（既に設定済みの env var は上書きしない。
    // `DATABASE_URL= just schema-apply-dsql` のような無効化はそのまま効く）
    let _ = dotenvy::dotenv();
    match Cli::parse().command {
        Command::Diff {
            name,
            schema,
            migrations,
            scratch_db,
            enable_drop,
            pg,
        } => diff(&name, &schema, &migrations, &scratch_db, enable_drop, &pg).await,
        Command::Apply { migrations } => apply(&migrations).await,
        Command::RebuildLab { schema, lab_db, pg } => rebuild_lab(&schema, &lab_db, &pg).await,
        Command::Export {
            db,
            dsql,
            check,
            write,
            schema,
            pg,
        } => export(&db, dsql, check, write.as_deref(), &schema, &pg).await,
        Command::Status => status().await,
    }
}

async fn diff(
    name: &str,
    schema: &Path,
    migrations_dir: &Path,
    scratch_db: &str,
    enable_drop: bool,
    pg: &LocalPg,
) -> anyhow::Result<()> {
    // baseline: migrations/ を空の scratch に再生する（履歴テーブルは作らない）。
    // これにより「migrations の積み重ね ⊕ 今回の diff = schema.sql」の整合が毎回検査される
    pg.recreate(scratch_db).await?;
    let db = connect::connect(&DbSource::Direct(pg.url(scratch_db))).await?;
    let migrations = applier::load_migrations(migrations_dir)?;
    applier::apply_migrations(
        &db,
        &migrations,
        &ApplyOptions {
            is_dsql: false,
            record_history: false,
        },
    )
    .await
    .context("baseline 再生に失敗しました (migrations/*.sql が不整合の可能性)")?;

    let desired = std::fs::read_to_string(schema)
        .with_context(|| format!("スキーマファイルを読めません: {}", schema.display()))?;
    let raw = psqldef::dry_run(&pg.psqldef_target(scratch_db), &desired, enable_drop)?;
    let parsed = dsqlize::parse_dry_run(&raw);
    if !parsed.skipped.is_empty() {
        bail!(
            "破壊的変更が検出されました。意図した変更なら --enable-drop を付けて再実行してください:\n{}",
            parsed.skipped.join("\n")
        );
    }
    // DROP CONSTRAINT の種別 (CHECK / UNIQUE のみ可) は baseline (= scratch DB) の catalog で判定する。
    // ファイル書き出しより前に完了する (止まったときに migration を残さない)
    let statements = dsqlize::dsqlize_with_baseline(&db, &parsed.statements).await?;
    if statements.is_empty() {
        println!(
            "差分なし: {} は migrations/ の再生結果と一致しています",
            schema.display()
        );
        return Ok(());
    }

    let version = format!("{}_{name}.sql", chrono::Utc::now().format("%Y%m%d%H%M%S"));
    let path = migrations_dir.join(&version);
    let body: Vec<String> = statements.iter().map(|s| format!("{s};\n")).collect();
    let content = format!(
        "-- {name} (dsql-schema-migrate diff で生成。適用前にレビューすること)\n\n{}",
        body.join("\n")
    );
    std::fs::write(&path, &content)
        .with_context(|| format!("migration の書き込みに失敗: {}", path.display()))?;
    println!("生成: {}\n", path.display());
    print!("{content}");
    Ok(())
}

async fn apply(migrations_dir: &Path) -> anyhow::Result<()> {
    let source = connect::from_env()?;
    let db = connect::connect(&source).await?;
    let migrations = applier::load_migrations(migrations_dir)?;
    let applied = applier::apply_migrations(
        &db,
        &migrations,
        &ApplyOptions {
            is_dsql: source.is_dsql(),
            record_history: true,
        },
    )
    .await?;
    if applied.is_empty() {
        println!("適用済み: 新しいマイグレーションはありません");
    } else {
        for version in &applied {
            println!("適用: {version}");
        }
        if source.is_dsql() {
            println!("注意: ASYNC index はバックグラウンドで作成されます (status で確認)");
        }
    }
    Ok(())
}

async fn rebuild_lab(schema: &Path, lab_db: &str, pg: &LocalPg) -> anyhow::Result<()> {
    let sql = std::fs::read_to_string(schema)
        .with_context(|| format!("スキーマファイルを読めません: {}", schema.display()))?;
    pg.recreate(lab_db).await?;
    let db = connect::connect(&DbSource::Direct(pg.url(lab_db))).await?;
    applier::execute_sql(&db, &sql).await?;
    println!("再構築: {lab_db} ({} から)", schema.display());
    Ok(())
}

async fn export(
    db: &str,
    dsql: bool,
    check: bool,
    write: Option<&Path>,
    schema: &Path,
    pg: &LocalPg,
) -> anyhow::Result<()> {
    let raw = if dsql {
        let source = connect::from_env()?;
        let DbSource::Dsql { host, user, region } = source else {
            bail!("--dsql には DSQL_HOST 環境変数が必要です (DATABASE_URL は未設定にする)");
        };
        let token = connect::generate_token(&host, &region, &user).await?;
        psqldef::export(&psqldef::Target {
            host,
            port: 5432,
            user,
            dbname: "postgres".to_owned(),
            password: Some(token),
            ssl: true,
        })?
    } else {
        psqldef::export(&pg.psqldef_target(db))?
    };
    let normalized = dsqlize::normalize_export(&raw, dsql);

    if check {
        let want = std::fs::read_to_string(schema)
            .with_context(|| format!("比較対象を読めません: {}", schema.display()))?;
        if normalized == want {
            println!("ドリフトなし: {} と一致しています", schema.display());
            return Ok(());
        }
        let drift_path = schema.with_extension("drift.sql");
        std::fs::write(&drift_path, &normalized)
            .with_context(|| format!("書き込みに失敗: {}", drift_path.display()))?;
        return Err(DriftDetected {
            schema: schema.to_owned(),
            drift_path,
        }
        .into());
    }
    if let Some(path) = write {
        std::fs::write(path, &normalized)
            .with_context(|| format!("書き込みに失敗: {}", path.display()))?;
        println!("出力: {}", path.display());
    } else {
        print!("{normalized}");
    }
    Ok(())
}

async fn status() -> anyhow::Result<()> {
    let source = connect::from_env()?;
    let db = connect::connect(&source).await?;
    let backend = db.get_database_backend();
    let rows = db
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT version, applied_at::text AS applied_at FROM schema_migrations ORDER BY version",
        ))
        .await
        .context("schema_migrations を読めません (未適用の DB?)")?;
    println!("schema_migrations:");
    for row in rows {
        let version: String = row.try_get("", "version")?;
        let applied_at: String = row.try_get("", "applied_at")?;
        println!("  {version}  (applied_at: {applied_at})");
    }
    if source.is_dsql() {
        let rows = db
            .query_all_raw(Statement::from_string(
                backend,
                "SELECT status::text AS status, count(*) AS count FROM sys.jobs GROUP BY status",
            ))
            .await?;
        println!("sys.jobs (ASYNC index ジョブ):");
        if rows.is_empty() {
            println!("  (なし)");
        }
        for row in rows {
            let status: String = row.try_get("", "status")?;
            let count: i64 = row.try_get("", "count")?;
            println!("  {status}: {count}");
        }
    }
    Ok(())
}

/// clap の定義が壊れていないことを検査する（サブコマンド・既定値の取り違えを防ぐ）。
#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_定義は妥当() {
        Cli::command().debug_assert();
    }

    /// pg_user の env は crate 固有であること。汎用の `USER` を読むと、
    /// ログイン名が PG ユーザーとして採用され既定値 `postgres` が機能しなくなる。
    #[test]
    fn pg_user_の_env_は_crate_固有の名前() {
        let cmd = Cli::command();
        let diff = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "diff")
            .expect("diff サブコマンドがある");
        let pg_user = diff
            .get_arguments()
            .find(|a| a.get_id() == "pg_user")
            .expect("--pg-user がある");
        let env = pg_user.get_env().expect("env が設定されている");
        assert_eq!(env, "DSQL_SCHEMA_PG_USER");
    }
}
