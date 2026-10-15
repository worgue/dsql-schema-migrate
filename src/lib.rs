//! Aurora DSQL 向けスキーマ管理ライブラリ + CLI。
//!
//! psqldef を diff エンジンとしてのみ使う Aurora DSQL 標準構成の実装:
//!
//! - 信頼の源は `schema.sql`（素の PG 方言。psqldef export の正規形を
//!   [`dsqlize::normalize_export`] に通した形で管理する）
//! - diff 生成: migrations/ をローカル PG の scratch DB に再生（baseline）し、
//!   `psqldef --dry-run` の出力を [`dsqlize`] で DSQL 方言へ機械変換する。
//!   変換できない文（制約付き ADD COLUMN、ADD CONSTRAINT 等）は fail-loud で止まるので
//!   手書きに切り替える。`DROP CONSTRAINT` は baseline の catalog で種別を確認し、
//!   DSQL で drop できると実測済みの CHECK だけ通す（[`dsqlize::verify_drop_constraints`]）
//! - 適用: [`applier`] が migrations/*.sql を 1 文ずつ autocommit で実行する
//!   （DSQL は 1 トランザクション 1 DDL。適用履歴は `schema_migrations` テーブル）。
//!   ローカル PG へは `CREATE INDEX ASYNC` を同期 `CREATE INDEX` に変換して適用するため、
//!   テスト DB 構築と本番適用が同一コードになる
//!
//! baseline を「git HEAD の schema.sql」ではなく「migrations/ の再生結果」にしているのは、
//! migrations ⊕ 生成 diff = schema.sql の整合が diff のたびに検査されるようにするため
//! （migrations の手直しと schema.sql の乖離を見逃さない）。

pub mod applier;
pub mod cli;
pub mod connect;
pub mod dsqlize;
pub mod psqldef;

/// CLI を同期文脈から起動する（内部で tokio ランタイムを構築する）。
/// 消費プロジェクトの薄い bin（`src/bin/dsql_schema_migrate.rs`）はこれを呼ぶだけでよい。
///
/// プロセスの終了コードへの変換はここだけが行う（lib のコードからプロセスを殺さない）。
/// `export --check` のドリフトは exit 2、他のエラーは Err を返して呼び出し元の
/// main が exit 1 にする。
pub fn run_cli() -> anyhow::Result<()> {
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(cli::run());
    if let Err(err) = &result
        && err.downcast_ref::<cli::DriftDetected>().is_some()
    {
        eprintln!("{err}");
        std::process::exit(cli::DRIFT_EXIT_CODE);
    }
    result
}
