//! psqldef（sqldef）サブプロセスの薄いラッパー。
//!
//! psqldef は diff エンジンとしてのみ使う（DB への適用は applier の責務）。
//! バイナリは PATH の `psqldef` を使い、`DSQL_SCHEMA_PSQLDEF` で上書きできる。

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Context;

/// psqldef の接続先。
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub dbname: String,
    /// DSQL のときは IAM トークン（PGPASSWORD として渡す）
    pub password: Option<String>,
    /// DSQL のときは true（PGSSLMODE=require を渡す）
    pub ssl: bool,
}

fn command(target: &Target) -> Command {
    let bin = std::env::var("DSQL_SCHEMA_PSQLDEF").unwrap_or_else(|_| "psqldef".to_owned());
    let mut cmd = Command::new(bin);
    cmd.arg("-h")
        .arg(&target.host)
        .arg("-p")
        .arg(target.port.to_string())
        .arg("-U")
        .arg(&target.user)
        .arg(&target.dbname);
    if let Some(password) = &target.password {
        cmd.env("PGPASSWORD", password);
    }
    if target.ssl {
        cmd.env("PGSSLMODE", "require");
    }
    cmd
}

/// psqldef を実行して stdout を返す。stdin に desired SQL を渡せる。
///
/// psqldef の generic parser はカラム名 `text` 等で WARN を吐いて pgquery に
/// フォールバックする（実害なし）ため、stderr の WARN 行は抑制する。
fn run(mut cmd: Command, stdin: Option<&str>) -> anyhow::Result<String> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context(
        "psqldef を起動できません。PATH に psqldef を導入してください \
             (導入手順は README。DSQL_SCHEMA_PSQLDEF でバイナリのパスを指定することもできます)",
    )?;
    if let Some(desired) = stdin {
        child
            .stdin
            .take()
            .context("psqldef の stdin を取得できません")?
            .write_all(desired.as_bytes())
            .context("psqldef への書き込みに失敗しました")?;
    }
    let output = child
        .wait_with_output()
        .context("psqldef の実行に失敗しました")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let noise_free: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.contains("WARN Generic parser failed"))
        .collect();
    if !output.status.success() {
        anyhow::bail!("psqldef が失敗しました:\n{}", noise_free.join("\n"));
    }
    for line in noise_free {
        eprintln!("psqldef: {line}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `psqldef --export`: 現在のスキーマを出力する。
pub fn export(target: &Target) -> anyhow::Result<String> {
    let mut cmd = command(target);
    cmd.arg("--export");
    run(cmd, None)
}

/// `psqldef --dry-run`: desired SQL との差分 DDL を出力する（DB は変更しない）。
pub fn dry_run(target: &Target, desired: &str, enable_drop: bool) -> anyhow::Result<String> {
    let mut cmd = command(target);
    cmd.arg("--dry-run");
    if enable_drop {
        cmd.arg("--enable-drop");
    }
    run(cmd, Some(desired))
}
