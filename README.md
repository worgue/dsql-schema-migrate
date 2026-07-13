# dsql-schema-migrate

Aurora DSQL 向けスキーマ管理ツール（psqldef diff + DSQL 方言変換 + 自前 applier）。

Aurora DSQL は「PostgreSQL 互換」だが、FK 制約なし・`ON CONFLICT` なし・
`CREATE INDEX ASYNC`・1 トランザクション 1 DDL など通常の PG と異なる制約が多く、
既存のマイグレーションツールがそのまま使えない。dsql-schema-migrate は
[psqldef](https://github.com/sqldef/sqldef) を **diff エンジンとしてのみ**使い、
適用は 1 文ずつ autocommit の自前 applier で行うことでこの制約に適合する。

## 仕組み

- 信頼の源は `schema.sql`（素の PG 方言。psqldef export の正規形で管理する）
- **diff 生成**: `migrations/` をローカル PostgreSQL の scratch DB に再生（baseline）し、
  `psqldef --dry-run` の出力を DSQL 方言へ機械変換する。変換できない文
  （制約付き ADD COLUMN 等）は fail-loud で止まるので手書きに切り替える
- **適用**: applier が `migrations/*.sql` を 1 文ずつ autocommit で実行する
  （適用履歴は `schema_migrations` テーブル）。ローカル PG へは `CREATE INDEX ASYNC` を
  同期 `CREATE INDEX` に変換して適用するため、テスト DB 構築と本番適用が同一コードになる
- baseline を「migrations/ の再生結果」にしているため、migrations と schema.sql の
  不整合は diff のたびに検査される

## 導入

lib + bin 両対応。推奨は **git 依存 + 薄い bin**（テストから applier を lib として使う場合、
CLI と lib のバージョンが Cargo.toml の 1 行で常に一致する）:

```toml
# Cargo.toml
[dependencies]
dsql-schema-migrate = { git = "https://github.com/worgue/dsql-schema-migrate.git", rev = "<rev>" }
```

```rust
// src/bin/dsql_schema_migrate.rs
fn main() -> anyhow::Result<()> {
    dsql_schema_migrate::run_cli()
}
```

バイナリだけ欲しい場合は `cargo install` でも導入できる:

```sh
cargo install --git https://github.com/worgue/dsql-schema-migrate.git --rev <rev> dsql-schema-migrate
```

前提ツール: psqldef（diff / export に使用。version pin での導入を推奨）と
ローカル PostgreSQL（scratch / lab DB 用）:

```sh
version="v3.11.13"
arch=$(uname -m | sed 's/aarch64/arm64/; s/x86_64/amd64/')
curl -sL "https://github.com/sqldef/sqldef/releases/download/${version}/psqldef_linux_${arch}.tar.gz" \
  | tar xz -C ~/.local/bin psqldef
```

lib として使う場合のクレート名は `dsql_schema_migrate`。

## コマンド

| コマンド | 入力 | 出力 |
|---|---|---|
| `diff <name>` | schema.sql + migrations/ + ローカル PG | `migrations/<ts>_<name>.sql`（DSQL 方言） |
| `apply` | migrations/ + 接続先 env | 適用ログ（DB の `schema_migrations` 更新） |
| `rebuild-lab` | schema.sql + ローカル PG | lab DB（entity 生成用） |
| `export` | 接続先（lab または `--dsql`） | 正規形スキーマ（`--check` でドリフト検査） |
| `status` | 接続先 env | `schema_migrations` / `sys.jobs`（ASYNC index） |

接続先は環境変数で解決する: `DATABASE_URL`（直接 URL）>
`DSQL_HOST`（IAM 認証。`DSQL_USER` 既定 `admin`、リージョンは `AWS_REGION` 既定
`ap-northeast-1`）。IAM トークンは接続のたびに生成し、キャッシュしない。

## 典型的なワークフロー

1. `schema.sql` を編集する（FK / SERIAL は書かない。DSQL が受け付けない）
2. `dsql-schema-migrate diff <name>` — baseline（migrations/ の再生結果）との差分から
   `migrations/<ts>_<name>.sql` が生成される。機械変換できない変更は fail-loud で
   止まるので、テーブル再作成パターン（新テーブル → データコピー → リネーム）を
   手書きする。破壊的変更（DROP）は既定でエラー。意図したものだけ `--enable-drop` を付ける
3. 生成された migration をレビューする（runner への登録作業は不要。migrations/
   ディレクトリが正）
4. `DSQL_HOST=<endpoint> dsql-schema-migrate apply` で適用 → `status` で
   ASYNC index ジョブを確認 → `export --dsql --check` でドリフト検査
5. entity 生成が要るプロジェクトは `rebuild-lab` で lab DB を再構築してから生成する

途中で apply が失敗しても、文単位チェックポイントにより再実行は失敗した文から再開する
（先頭からのやり直しで `already exists` に詰まらない）。

## テストからの利用

テスト DB 構築に applier を lib として使うと、本番適用と同一コードでスキーマが立つ:

```rust
let migrations = dsql_schema_migrate::applier::load_migrations(Path::new("migrations"))?;
dsql_schema_migrate::applier::apply_migrations(
    &db,
    &migrations,
    &dsql_schema_migrate::applier::ApplyOptions { is_dsql: false, record_history: false },
).await?;
```

## License

MIT
