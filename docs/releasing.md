# リリース手順

`dsql-schema-migrate` の binary を GitHub Releases へ置くときの手順。
**tag `X.Y.Z` の push で GitHub Actions (`.github/workflows/release.yml`) が
各 target の binary をビルドし、Release オブジェクトの作成 (本文は CHANGELOG の該当節)
と tar.gz / sha256 の添付までを行う**。tag に `v` は付けない。

役割は 2 つに分かれる:

- **リリースの準備 (LLM が dev で行う)**: version のバンプ・CHANGELOG の確定・リリースコミット・
  dev への push (手順 1〜5)
- **昇格とリリース (op が host で行う)**: `just ship-prod` (手順 6)。origin/dev 先端の CI を
  検証し、origin/main を ff して、`Cargo.toml` の version の tag がまだ無ければ tag を打って
  push する。この repo は deploy 先を持たないので、prod 昇格の最後が deploy ではなくリリースになる

`just ship-prod` は版を決めない。`Cargo.toml` の version の tag が既にあれば main の ff だけを行い
(未リリースの変更を main に届ける)、無ければその版をリリースする。

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
6. **op に `just ship-prod` を依頼する** (host の main worktree で実行)。次を順に行う:
   - origin/dev 先端の CI (`ci.yml`) が success であることを確かめる (実行中なら待つ)
   - origin/main を ff する
   - tag `X.Y.Z` を打って push する (`release.yml` が発火)
   - release の run の完走と、asset が 4 target × (tar.gz + sha256) = 8 個あることを確かめる

   先に内容だけ見るなら `DRY_RUN=1 just ship-prod` (push を表示だけにする)。
   リリースコミットの後ろに変更を積んだ場合は、`[Unreleased]` に未確定の変更が残るので
   ship-prod が止まる。リリースコミットの sha を渡す (`just ship-prod <sha>`) と、そこまでを
   昇格してリリースする (その commit が push の先端だったときの CI の run が要る)
7. **Release を確認する**: `gh release view X.Y.Z` で本文と asset を確認し、依頼元へ返答する

## 失敗したとき

- tag の `git push` が `! [remote rejected] ... (fatal error in commit_refs_rules)` で拒否される
  (GitHub 側のエラー。GitHub status は正常・repo の ruleset は branch 向けのみ、という状態でも
  起きた。再試行しても通らない): REST API で tag を作ると通る。`release.yml` は API 経由の
  ref 作成でも push event として発火する
  ```sh
  sha=$(git rev-parse X.Y.Z^{commit})
  tag=$(gh api repos/worgue/dsql-schema-migrate/git/tags -f tag=X.Y.Z -f message=X.Y.Z -f object=$sha -f type=commit --jq .sha)
  gh api repos/worgue/dsql-schema-migrate/git/refs -f ref=refs/tags/X.Y.Z -f sha=$tag
  git tag -d X.Y.Z && git fetch origin tag X.Y.Z   # ローカルを remote の tag オブジェクトに揃える
  ```
- `just ship-prod` が途中で止まった: 表示された原因を直して `just ship-prod` を再実行する。
  済んだ段階 (main の ff・手元の tag) は飛ばして続きから進む。初回や ship.sh を更新した直後は
  「この ship.sh は origin/dev の版と違います」で止まるので、案内どおり main worktree で
  `git merge --ff-only origin/dev` してから実行し直す
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
    日常の作業とリリースコミットは dev 上で行う。main は dev を ff で追う昇格ポインタで、
    op の `just ship-prod` だけが進める (tag も ship-prod が main の先端と同じ commit に打つ)。
    LLM は main と tag を push しない。
