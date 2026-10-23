# リリース手順

`dsql-schema-migrate` の binary を GitHub Releases へ置くときの手順。
**tag `X.Y.Z` の push で GitHub Actions (`.github/workflows/release.yml`) が
各 target の binary をビルドし、Release オブジェクトの作成 (本文は CHANGELOG の該当節)
と tar.gz / sha256 の添付までを行う**。magic-pocket と同じ流儀 (tag に `v` は付けない。
tag は dev 上の commit に打つ。リリース作業は LLM が行う)。

配布 target:

| target | runner | 用途 |
|---|---|---|
| `x86_64-unknown-linux-gnu` | ubuntu-latest | GHA の ubuntu runner など (glibc は runner のものに依存) |
| `x86_64-unknown-linux-musl` | ubuntu-latest | 静的リンク。古い glibc の環境や alpine 系 |
| `aarch64-unknown-linux-musl` | ubuntu-24.04-arm | 静的リンク。arm64 の VM / コンテナ (forge VM 等) |
| `aarch64-apple-darwin` | macos-latest | op の host (Apple Silicon) |

musl target は `musl-tools`（musl-gcc）だけで通る（依存の aws-lc-sys は cmake + musl-gcc で
自前ビルドされる。特別な `CC` 指定は不要。runner には cmake が最初から入っている）。musl の
ビルドが落ちたら、まず runner image の cmake / musl-tools の有無を疑う。ローカル再現は README「開発」。

## バージョンの決め方 (SemVer)

- **バグ修正のみ** → patch (例: `0.2.1` → `0.2.2`)
- **後方互換な新機能** → minor (例: `0.3.0` → `0.4.0`)。0.x でも feature は minor
- **破壊的変更** (公開 API / CLI の互換を壊す、依存 crate の major を上げる等) → 0.x では
  minor (1.0 以降は major)。CHANGELOG に **破壊的** と明記する

## 手順

1. **version をバンプする**: `Cargo.toml` の `version` (tag と一致が必須。`release.yml` の
   "Check tag matches Cargo.toml version" が検証する)。`cargo check` で `Cargo.lock` にも
   反映する (`dsql-schema-migrate` 自身の version 行が変わる)
2. **CHANGELOG を確定する**: `## [Unreleased]` の下の内容を
   `## [X.Y.Z](https://github.com/worgue/dsql-schema-migrate/releases/tag/X.Y.Z) - YYYY-MM-DD`
   の節に移し、空の `## [Unreleased]` を残す。`release.yml` はこの節見出しで本文を切り出すので、
   見出しの形を崩さない (節が無いと release job が失敗する)
3. **検査を通す**: `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`
4. **コミットする**: `:bookmark: X.Y.Z リリース (<要約>)`
5. **dev を push する**: `git push origin dev`。CI (`ci.yml`) が green になるのを待つ
6. **tag を作成して push する**: `git tag -a X.Y.Z -m "X.Y.Z"` → `git push origin X.Y.Z`。
   これで `release.yml` が発火する
7. **Release を確認する**: `gh run watch <run-id>` で workflow 成功を確認し、
   `gh release view X.Y.Z` で asset が 4 target × (tar.gz + sha256) = 8 個あることを確認する

## 失敗したとき

- `check` で止まった (tag と version の不一致): tag を消して (`git push --delete origin X.Y.Z` →
  `git tag -d X.Y.Z`) version を直し、同じ tag を打ち直す。Release オブジェクトはまだ無い
- `build` / `release` で止まった: 原因を直して dev に commit し、tag を打ち直す
  (`git tag -fa X.Y.Z` → `git push -f origin X.Y.Z`)。Release が既にできていれば
  release job は本文を更新して asset を `--clobber` で差し替える。**消費側がすでに
  その tag を参照しているなら打ち直さず patch 版を切る**

## 消費側への案内

- binary: `https://github.com/worgue/dsql-schema-migrate/releases/download/X.Y.Z/dsql-schema-migrate-<target>.tar.gz`
  (同じ場所に `.sha256`)。README「導入」の curl 例を参照
- crate (lib として使う側): git 依存の `rev = "<hash>"` の代わりに `tag = "X.Y.Z"` が使える。
  binary と lib の版を揃えるときは tag で揃える
- 変更を入れた feedback の返信には、rev を進める判断材料として対応内容と commit hash
  (リリース済みなら tag) を書く

!!! note "dev / main の運用について"
    日常の作業・リリースコミット・tag はすべて dev 上で行う。main / stg / prod への昇格は
    人間が行う (`release.yml` は tag push で発火するため branch は問わない)。
