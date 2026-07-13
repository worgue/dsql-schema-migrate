//! 接続層。IAM 認証トークンは接続のたびに生成し、キャッシュしない（有効期限 15 分のため）。
//!
//! CLI は一発実行なのでプールの短命化設定は不要（アプリ本体の接続層は各プロジェクトの
//! db.rs が持つ。ここはスキーマ操作専用の薄い接続）。

use anyhow::Context;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

/// 接続先の解決結果。
pub enum DbSource {
    /// 接続 URL 直接指定（ローカル PG 等）
    Direct(String),
    /// Aurora DSQL（IAM 認証）
    Dsql {
        host: String,
        user: String,
        region: String,
    },
}

impl DbSource {
    pub fn is_dsql(&self) -> bool {
        matches!(self, DbSource::Dsql { .. })
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// 環境変数から接続先を解決する。優先順位は
/// `DATABASE_URL` > `DSQL_HOST` / `POCKET_DSQL_ENDPOINT`（空文字列は未設定扱い）。
pub fn from_env() -> anyhow::Result<DbSource> {
    if let Some(url) = env_nonempty("DATABASE_URL") {
        return Ok(DbSource::Direct(url));
    }
    if let Some(host) = env_nonempty("DSQL_HOST").or_else(|| env_nonempty("POCKET_DSQL_ENDPOINT")) {
        return Ok(DbSource::Dsql {
            host,
            user: env_nonempty("DSQL_USER").unwrap_or_else(|| "admin".to_owned()),
            region: env_nonempty("AWS_REGION")
                .or_else(|| env_nonempty("POCKET_DSQL_REGION"))
                .unwrap_or_else(|| "ap-northeast-1".to_owned()),
        });
    }
    anyhow::bail!("DATABASE_URL または DSQL_HOST 環境変数が必要です")
}

/// DSQL の IAM 認証トークンを生成する（有効期限 最大 15 分）。
pub async fn generate_token(host: &str, region: &str, user: &str) -> anyhow::Result<String> {
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;

    let token_config = aws_sdk_dsql::auth_token::Config::builder()
        .hostname(host)
        .region(aws_types::region::Region::new(region.to_owned()))
        .expires_in(900)
        .build()
        .map_err(|e| anyhow::anyhow!("DSQL トークン設定の構築に失敗しました: {e}"))?;

    let signer = aws_sdk_dsql::auth_token::AuthTokenGenerator::new(token_config);

    // admin / 非 admin でトークン生成 API が異なる
    let token = if user == "admin" {
        signer.db_connect_admin_auth_token(&sdk_config).await
    } else {
        signer.db_connect_auth_token(&sdk_config).await
    }
    .map_err(|e| anyhow::anyhow!("DSQL 認証トークン生成に失敗しました: {e}"))?;

    Ok(token.to_string())
}

/// DbSource に応じて接続する。
pub async fn connect(source: &DbSource) -> anyhow::Result<DatabaseConnection> {
    let url = match source {
        DbSource::Direct(url) => url.clone(),
        DbSource::Dsql { host, user, region } => {
            let token = generate_token(host, region, user).await?;
            // トークンは記号を含むため URL エンコード必須
            let encoded = urlencoding::encode(&token);
            format!("postgres://{user}:{encoded}@{host}:5432/postgres?sslmode=require")
        }
    };
    let mut opt = ConnectOptions::new(&url);
    opt.max_connections(1).sqlx_logging(false);
    Database::connect(opt)
        .await
        .context("データベース接続に失敗しました")
}

/// ローカル PG の接続 URL を組み立てる。
pub fn local_url(user: &str, host: &str, port: u16, dbname: &str) -> String {
    format!("postgres://{user}@{host}:{port}/{dbname}?sslmode=disable")
}

/// ローカル PG のデータベースを作り直す（存在すれば強制 DROP）。
pub async fn recreate_local_db(
    user: &str,
    host: &str,
    port: u16,
    dbname: &str,
) -> anyhow::Result<()> {
    let admin = connect(&DbSource::Direct(local_url(user, host, port, "postgres"))).await?;
    use sea_orm::ConnectionTrait;
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
        .await
        .with_context(|| format!("DROP DATABASE {dbname} に失敗しました"))?;
    admin
        .execute_unprepared(&format!("CREATE DATABASE {dbname}"))
        .await
        .with_context(|| format!("CREATE DATABASE {dbname} に失敗しました"))?;
    Ok(())
}
