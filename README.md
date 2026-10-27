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
  （制約付き ADD COLUMN、ADD CONSTRAINT 等）は fail-loud で止まるので手書きに切り替える。
  `DROP CONSTRAINT` は baseline の catalog で制約の種別を確認し、DSQL で drop できると
  実測済みの CHECK 制約だけ通す（PRIMARY KEY / UNIQUE は未実測のため止める）
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

バイナリだけ欲しい場合は GitHub Releases のビルド済み binary を使う (tag `X.Y.Z` ごとに
`x86_64-unknown-linux-gnu` / `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` /
`aarch64-apple-darwin` の tar.gz と sha256 を置いている。リリース手順は
[docs/releasing.md](docs/releasing.md)):

```sh
ver=X.Y.Z target=x86_64-unknown-linux-musl
curl -sSL -o /tmp/dsm.tar.gz \
  https://github.com/worgue/dsql-schema-migrate/releases/download/${ver}/dsql-schema-migrate-${target}.tar.gz
tar xzf /tmp/dsm.tar.gz -C /tmp
install -m 755 /tmp/dsql-schema-migrate-${target}/dsql-schema-migrate ~/.local/bin/
```

ソースからビルドする場合は `cargo install` でも導入できる (`--rev` の代わりに `--tag X.Y.Z` も可):

```sh
cargo install --locked --git https://github.com/worgue/dsql-schema-migrate.git --tag X.Y.Z dsql-schema-migrate
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
   手書きする。破壊的変更（DROP TABLE 等）は既定でエラー。意図したものだけ `--enable-drop` を付ける
   （CHECK 制約の `DROP CONSTRAINT` はデータ喪失がないため、ゲートなしで生成される）
3. 生成された migration をレビューする（runner への登録作業は不要。migrations/
   ディレクトリが正）
4. `DSQL_HOST=<endpoint> dsql-schema-migrate apply` で適用 → `status` で
   ASYNC index ジョブを確認 → `export --dsql --check` でドリフト検査
5. entity 生成が要るプロジェクトは `rebuild-lab` で lab DB を再構築してから生成する

途中で apply が失敗しても、文単位チェックポイントにより再実行は失敗した文から再開する
（先頭からのやり直しで `already exists` に詰まらない）。DSQL の OCC 競合（`OC001`
schema has been updated by another transaction 等。新しいクラスターに多数の
`CREATE TABLE` / `CREATE INDEX ASYNC` を続けて流す初回 bootstrap で、ASYNC index ジョブの
schema 更新と次の DDL が衝突して起きる）は、失敗した文だけを指数バックオフで自動再試行する
（最大 6 回・合計約 12 秒。stderr に再試行ログを出す）。それでも解消しなければ再開の案内付きで止まる。

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

## 開発

`cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test` を通す。unit test は
ローカル PostgreSQL なしで動く。`diff` の結合テスト（`tests/`）と applier の DB テスト
（チェックポイント再開・OCC 再試行）はローカル PG（`diff` は PATH の psqldef も）が要るため、
`DSQL_SCHEMA_TEST_PG_HOST`（例: `127.0.0.1`。ユーザーは `DSQL_SCHEMA_TEST_PG_USER`、
既定 `postgres`）を設定したときだけ走り、未設定なら skip する。CI では PostgreSQL service を立てて
必ず走らせている。

開発 VM（Ubuntu）でローカルに走らせる手順（結合テストと `diff` の実機確認用）:

```sh
# PostgreSQL 16 を入れ、TCP (127.0.0.1) をパスワードなし (trust) で受けるようにする
sudo apt-get install -y postgresql
sudo sed -i 's|^\(host\s\+all\s\+all\s\+127.0.0.1/32\s\+\)scram-sha-256|\1trust|' /etc/postgresql/16/main/pg_hba.conf
sudo pg_ctlcluster 16 main reload   # 停止していれば start

# psqldef (sqldef) のリリース tarball を PATH に置く (version は CI の ci.yml と揃える)
curl -sSL -o /tmp/psqldef.tar.gz \
  https://github.com/sqldef/sqldef/releases/download/v3.11.26/psqldef_linux_amd64.tar.gz
tar xzf /tmp/psqldef.tar.gz -C /tmp psqldef && install -m 755 /tmp/psqldef ~/.local/bin/psqldef

DSQL_SCHEMA_TEST_PG_HOST=127.0.0.1 cargo test
```

配布 binary（`release.yml` の musl 静的ビルド）をローカルで再現する手順。aws-lc-sys の C ビルドが
入るが、`musl-tools`（musl-gcc）と `cmake` があれば特別な `CC` 指定なしで通る。VM の arch と同じ
target しかリンクできない（arm64 VM なら `aarch64-unknown-linux-musl`。x86_64 向けは GHA 側で確認）:

```sh
sudo apt-get install -y musl-tools cmake
rustup target add aarch64-unknown-linux-musl     # x86_64 の VM なら x86_64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
target/aarch64-unknown-linux-musl/release/dsql-schema-migrate --version
```

DSQL 本体（AWS）はこの repo の開発環境には無い。DSQL 固有の挙動（DDL が通るか等）の実測は、
DSQL の sandbox を持つ消費側 project に feedback で手順を書いて依頼し、結果をコード内コメントと
CHANGELOG に「どの project の sandbox で何日に実測」として残す（固有情報は書かない）。

## License

MIT
