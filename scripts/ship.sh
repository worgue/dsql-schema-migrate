#!/usr/bin/env bash
# ship — prod 昇格 (op が host の main worktree で実行する)。
#
# この repo は deploy 先を持たない lib / CLI なので、昇格の最後は deploy ではなく
# **リリース** (tag X.Y.Z の push → release.yml が binary をビルドして GitHub Release を作る)。
#
#   ship.sh prod [sha]  — origin/dev 先端 (sha 省略時) の CI を検証 → origin/main を ff →
#                         Cargo.toml の version の tag がまだ無ければ、その commit に tag を
#                         打って push し、release.yml の完走と asset を確認する。
#                         tag が既にあれば ff だけ (未リリースの変更を main に届ける用途)
#
#   [sha] は hex の commit SHA のみ (短縮可)。origin/dev に含まれる commit に限る。
#   リリースコミットの後ろに変更を積んだ dev から、リリースコミットだけを昇格するときに使う
#
# リリースの準備 (version のバンプ・CHANGELOG の確定・`:bookmark:` コミット・dev への push) は
# 事前に済ませておく (docs/releasing.md)。この script は版を決めない: Cargo.toml に書かれた
# version を、tag が無ければリリースするだけ。
#
# 環境変数:
#   DRY_RUN=1              push を表示のみにする (検証は行う)
#   SHIP_SKIP_CONFIRM=1    確認ゲートを無効にする (既定は ON)
#   TEST_WORKFLOW          昇格前に success を確かめる workflow ファイル名 (既定 ci.yml)
#   VERIFY_RUN_DEADLINE    release の run の完走を待つ上限秒 (既定 1800)
#   TEST_WAIT_DEADLINE     main ff の前に実行中の CI の run を待つ上限秒 (既定 1200)
#   RELEASE_RUN_DEADLINE   tag の push の後に release の run が現れるのを待つ上限秒 (既定 120)
#
# host の macOS (bash 3.2) で動く書き方に限る (配列・連想配列・${var,,} 等を使わない)。
set -euo pipefail

RELEASE_WORKFLOW="release.yml"
TEST_WORKFLOW="${TEST_WORKFLOW:-ci.yml}"
VERIFY_RUN_DEADLINE="${VERIFY_RUN_DEADLINE:-1800}"
TEST_WAIT_DEADLINE="${TEST_WAIT_DEADLINE:-1200}"
RELEASE_RUN_DEADLINE="${RELEASE_RUN_DEADLINE:-120}"
# release.yml の配布 target 4 つ × (tar.gz + sha256)
EXPECTED_ASSETS=8

MODE="${1:?サブコマンドが必要です (prod)}"
SHA_ARG="${2:-}"
# sha 引数は hex の commit SHA のみ。branch / tag 名を黙って解決しない
if [ -n "$SHA_ARG" ] && ! [[ "$SHA_ARG" =~ ^[0-9a-f]{7,40}$ ]]; then
  echo "ship: エラー: sha 引数は hex の commit SHA で指定してください (短縮可 / branch・tag 名は不可): $SHA_ARG" >&2
  exit 1
fi
DRY_RUN="${DRY_RUN:-}"
SHIP_SKIP_CONFIRM="${SHIP_SKIP_CONFIRM:-}"

SHA=""
FF_NEEDED=0
VERSION=""
# RELEASE_ACTION: none (tag は昇格範囲内にある = リリース済み) / tag (tag を打ってリリースする)
RELEASE_ACTION="none"
RUN_CONCLUSION=""

log() { echo "ship: $*"; }
fail() {
  echo "ship: エラー: $*" >&2
  exit 1
}

# 変更系コマンド (DRY_RUN=1 なら表示のみ)
run() {
  if [ -n "$DRY_RUN" ]; then
    echo "ship (dry-run): $*"
  else
    "$@"
  fi
}

short() { git rev-parse --short "$1"; }

# gh の一過性失敗 (token の mint 失敗・API の瞬断) を短い retry で吸収する
gh_retry() {
  local attempt out
  for attempt in 1 2 3; do
    if out="$(gh "$@")"; then
      echo "$out"
      return 0
    fi
    [ "$attempt" -lt 3 ] && sleep 5
  done
  return 1
}

# $1=run_id $2=表示名。完走を待って conclusion を RUN_CONCLUSION に入れる。
# `gh run watch` は完走前に戻ることがあり exit code も信用できないので、status が
# completed になるまで再 watch し、success かどうかは呼び元が conclusion で判定する
wait_run() {
  local run_id="$1" name="$2" status="" waited=0
  log "$name run $run_id の完走を待ちます (gh run watch)"
  while :; do
    gh run watch "$run_id" --exit-status || true
    status="$(gh run view "$run_id" --json status --jq .status || true)"
    [ "$status" = "completed" ] && break
    if [ "$waited" -ge "$VERIFY_RUN_DEADLINE" ]; then
      fail "$name の run $run_id が ${VERIFY_RUN_DEADLINE} 秒待っても完走しません (status: ${status:-不明})。gh run view $run_id で確認してください"
    fi
    log "$name run $run_id はまだ ${status:-不明} です。30 秒後に再 watch します"
    sleep 30
    waited=$((waited + 30))
  done
  RUN_CONCLUSION="$(gh_retry run view "$run_id" --json conclusion --jq .conclusion)" ||
    fail "$name の run $run_id の conclusion を取得できません (認証・ネットワークを確認して再実行してください)"
}

# --- 確認ゲート --------------------------------------------------------------

# リリース (tag の push) は消費側が参照する版を増やし、取り消しにくい操作なので、
# リリースするときだけ既定で人の確認を挟む (main の ff だけなら聞かない)。
# 非対話では黙って通さない
ship_confirm() {
  local answer
  if [ -n "$SHIP_SKIP_CONFIRM" ]; then
    log "確認ゲート: SHIP_SKIP_CONFIRM が設定されているためスキップします"
    return 0
  fi
  if [ -n "$DRY_RUN" ]; then
    log "(dry-run) 確認ゲートは省略します"
    return 0
  fi
  [ -t 0 ] || fail "確認ゲート: 対話端末で実行してください (確認を省くなら SHIP_SKIP_CONFIRM=1)"
  printf 'ship: %s をリリースしますか? [y/N] ' "$VERSION"
  read -r answer
  case "$answer" in
  y | Y | yes) ;;
  *) fail "中止しました (何も変更していません)" ;;
  esac
}

# --- 前提の検査 --------------------------------------------------------------

require_main_worktree() {
  local branch
  branch="$(git rev-parse --abbrev-ref HEAD)"
  [ "$branch" = "main" ] ||
    fail "main worktree で実行してください (現在のブランチ: $branch)。昇格は op が host で行います"
}

# 新しい ship.sh を dev に入れた直後の初回は、main worktree の checkout (= 古い main) の
# ship.sh で動いてしまう。origin/dev の版と違い、かつ HEAD が origin/dev を含まないなら、
# 手元の main だけを進めてから再実行するよう案内して止める
require_current_ship() {
  local self rel dev_blob local_blob
  self="${BASH_SOURCE[0]}"
  rel="$(git ls-files --full-name -- "$self" 2>/dev/null | head -n 1 || true)"
  [ -n "$rel" ] || return 0
  git merge-base --is-ancestor origin/dev HEAD && return 0
  dev_blob="$(git rev-parse --verify --quiet "origin/dev:$rel")" || return 0
  local_blob="$(git hash-object "$self")"
  [ "$dev_blob" = "$local_blob" ] && return 0
  fail "この ship.sh ($rel) は origin/dev の版と違います。手元の main だけを進めて、新しい版で実行し直してください (origin/main への push は再実行した ship-prod が検証を経て行います):
    git merge --ff-only origin/dev
    just ship-prod"
}

# ローカル dev (VM で commit された未 push 分も共有の .git で host から見える) が origin/dev に
# 押し切られているか。未 push があると「古い版を昇格する」事故になるので止める
require_dev_pushed() {
  local local_dev
  local_dev="$(git rev-parse --verify --quiet 'refs/heads/dev^{commit}')" || return 0
  git merge-base --is-ancestor "$local_dev" origin/dev ||
    fail "ローカル dev に origin/dev へ未 push の commit があります:
$(git log --oneline origin/dev..dev | sed 's/^/    /')
  dev worktree (VM) で \`git push origin dev\` を先に行ってください (未 push 分を含めず過去の commit を昇格するなら sha を明示します)"
}

# 昇格対象 SHA の解決。origin/main との位置関係で FF_NEEDED を決める:
#   - SHA が origin/main の祖先 (同位置を含む) → ff は不要 (リリースのやり直しだけがありうる)
#   - origin/main が SHA の祖先 → ff する
#   - 分岐 → 止める (ff-only の運用が壊れている。force push で直さない)
resolve_prod_sha() {
  local main
  main="$(git rev-parse origin/main)"
  if [ -n "$SHA_ARG" ]; then
    SHA="$(git rev-parse --verify --quiet "${SHA_ARG}^{commit}")" ||
      fail "commit が見つかりません: $SHA_ARG"
    git merge-base --is-ancestor "$SHA" origin/dev ||
      fail "$SHA が origin/dev に含まれません (昇格は dev へ push 済みの範囲から)"
  else
    require_dev_pushed
    SHA="$(git rev-parse origin/dev)"
  fi
  if git merge-base --is-ancestor "$SHA" "$main"; then
    FF_NEEDED=0
  elif git merge-base --is-ancestor "$main" "$SHA"; then
    FF_NEEDED=1
  else
    fail "origin/main ($(short "$main")) と $(short "$SHA") が分岐しています (dev を経ない commit が main に入った?)。force push せず \`git log --oneline $(short "$SHA")..origin/main\` で中身を確認してください"
  fi
  if ! git merge-base --is-ancestor HEAD "$SHA"; then
    git merge-base --is-ancestor "$SHA" HEAD ||
      fail "main worktree の HEAD と $(short "$SHA") が分岐しています。\`git status\` と \`git log --oneline $(short "$SHA")..HEAD\` で確認してください"
    # origin/main へは SHA:main を push するので、HEAD にだけある commit は main に載らない
    log "注意: main worktree の HEAD には $(short "$SHA") より先の commit があります (昇格対象には含めません):"
    git log --oneline "$SHA..HEAD" | sed 's/^/ship:     /'
  fi
}

# リリースの要否を決める。版は Cargo.toml (SHA 時点) の version で、この script は版を選ばない。
#   - tag X.Y.Z が origin にあり、その commit が SHA に含まれる → リリース済み (none)
#   - tag X.Y.Z が origin にあるのに SHA に含まれない → 止める (version の上げ忘れか、
#     tag より前の commit を指定した)
#   - tag が無い → リリースする (tag)。CHANGELOG に確定済みの節があり、[Unreleased] が空で
#     あること (リリースコミットの後ろに積んだ変更を、節に載らないまま同じ版で出さない)
resolve_release() {
  local tag_commit unreleased remote_tag
  VERSION="$(git show "$SHA:Cargo.toml" | sed -n 's/^version = "\([^"]*\)"/\1/p' | head -n 1)"
  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    fail "Cargo.toml ($(short "$SHA")) の version が X.Y.Z の形ではありません: ${VERSION:-(空)}"
  # tag の有無は origin で見る (手元にだけある tag = 前回 push に失敗した分は、未リリース扱い)
  remote_tag="$(git ls-remote --tags origin "refs/tags/$VERSION")" ||
    fail "origin の tag を取得できません (ネットワーク・認証を確認して再実行してください)"
  if [ -n "$remote_tag" ]; then
    tag_commit="$(git rev-parse --verify --quiet "refs/tags/$VERSION^{commit}")" ||
      fail "tag $VERSION が origin にありますが手元にありません。\`git fetch --tags origin\` の結果を確認してください"
    git merge-base --is-ancestor "$tag_commit" "$SHA" ||
      fail "tag $VERSION ($(short "$tag_commit")) が既にありますが、昇格対象 $(short "$SHA") に含まれません。Cargo.toml の version を上げ忘れていないか確認してください (docs/releasing.md)"
    RELEASE_ACTION="none"
    return 0
  fi
  git show "$SHA:CHANGELOG.md" | grep -q "^## \[$VERSION\]" ||
    fail "CHANGELOG.md ($(short "$SHA")) に '## [$VERSION]' の節がありません。リリースの準備 (docs/releasing.md の手順 1〜5) を dev で済ませてから再実行してください"
  unreleased="$(git show "$SHA:CHANGELOG.md" | awk '
    /^## \[Unreleased\]/ { on = 1; next }
    on && /^## / { exit }
    on && NF { print }
  ')"
  [ -z "$unreleased" ] ||
    fail "CHANGELOG.md ($(short "$SHA")) の [Unreleased] に未確定の変更があります。$VERSION のリリースコミットより後の変更です。リリースコミットの sha を明示して昇格する (just ship-prod <sha>) か、次の版としてリリースの準備をし直してください"
  RELEASE_ACTION="tag"
}

# 昇格の内容を表示する (確認ゲートの判断材料)。読むだけ
prod_announce() {
  if [ "$FF_NEEDED" = 1 ]; then
    log "origin/main..$(short "$SHA") を昇格します:"
    git log --oneline "$(git rev-parse origin/main)..$SHA" | sed 's/^/ship:     /'
  else
    log "origin/main は既に $(short "$SHA") を含みます (ff は不要)"
  fi
  if [ "$RELEASE_ACTION" = "tag" ]; then
    log "リリース: $VERSION を $(short "$SHA") に tag を打って出します (release.yml が binary をビルドして GitHub Release を作ります)"
  else
    log "リリース: なし ($VERSION は tag 済み。Cargo.toml の version が変わっていないので main の ff だけです)"
  fi
}

# --- 検証 --------------------------------------------------------------------

# SHA の CI (TEST_WORKFLOW) が success であることを確かめる。実行中なら完走を待つ。
# run が無い・success でないなら止める (検証していない commit を main に上げない)
verify_ci() {
  local wf name out run_id status conclusion
  for wf in $TEST_WORKFLOW; do
    name="${wf%.yml}"
    out="$(gh_retry run list --workflow="$wf" --commit="$SHA" -L 1 \
      --json databaseId,status,conclusion \
      --jq '.[0] | "\(.databaseId) \(.status) \(.conclusion)"')" ||
      fail "$name run の取得に失敗しました (gh が 3 回続けて失敗。host の gh 認証を確認してください)"
    read -r run_id status conclusion <<EOF
$out
EOF
    if [ -z "$run_id" ] || [ "$run_id" = "null" ]; then
      fail "$name の run が $(short "$SHA") にありません。dev へ push すると $wf が動きます (push 直後なら少し待って再実行。dev の途中の commit を sha で指定した場合は、その commit が push の先端だったときしか run がありません)"
    fi
    if [ "$status" != "completed" ]; then
      log "$name (run $run_id) が ${status:-不明} です。完走を待ちます (上限 ${TEST_WAIT_DEADLINE} 秒)"
      VERIFY_RUN_DEADLINE="$TEST_WAIT_DEADLINE" wait_run "$run_id" "$name"
      conclusion="$RUN_CONCLUSION"
    fi
    [ "$conclusion" = "success" ] ||
      fail "$name (run $run_id) が success ではありません: ${conclusion:-不明}。gh run view $run_id --log-failed で確認し、dev で直してから再実行してください"
    log "$name (run $run_id) は success です"
  done
}

# --- 昇格 --------------------------------------------------------------------

# origin/main を SHA へ ff する。.git は host と VM で共有なので、VM 向けの pre-push hook が
# host でも走りうる。押す SHA は dev への push 時に hook を通り CI も検証済みなので --no-verify
main_ff() {
  if [ "$FF_NEEDED" != 1 ]; then
    return 0
  fi
  run git push --no-verify origin "$SHA:refs/heads/main" ||
    fail "origin/main への ff push が失敗しました。CI は green で、未了なのは main の ff (とリリース) です。原因を直したら \`just ship-prod\` を再実行してください ('cannot lock ref' は別の push と重なっただけなので、そのまま再実行で通ります)"
  [ -n "$DRY_RUN" ] || log "origin/main を $(short "$SHA") へ ff しました"
}

# main worktree の checkout を SHA へ追随させる。HEAD が SHA を既に含むなら何もしない
checkout_follow() {
  git merge-base --is-ancestor "$SHA" HEAD && return 0
  run git merge --ff-only "$SHA" >/dev/null
  [ -n "$DRY_RUN" ] || log "main worktree の checkout を $(short "$SHA") へ進めました"
}

# $1=tag の push より前からある release run の id (空白区切り)。新しい run の id を返す
find_release_run() {
  local existing="$1" id
  for id in $(gh run list --workflow="$RELEASE_WORKFLOW" --commit="$SHA" --event=push -L 10 \
    --json databaseId --jq '.[].databaseId' 2>/dev/null || true); do
    case " $existing " in
    *" $id "*) ;;
    *)
      echo "$id"
      return 0
      ;;
    esac
  done
}

# asset の数を確かめる (release job が Release を作り終えたことの確認)
verify_release_assets() {
  local count
  count="$(gh_retry release view "$VERSION" --json assets --jq '.assets | length')" ||
    fail "Release $VERSION を取得できません。gh release view $VERSION で確認してください"
  [ "$count" = "$EXPECTED_ASSETS" ] ||
    fail "Release $VERSION の asset が $count 個です (期待は $EXPECTED_ASSETS 個)。release run の失敗 job を \`gh run rerun <run-id> --failed\` で再実行してください"
  log "Release $VERSION の asset は $EXPECTED_ASSETS 個そろっています"
}

# tag を打って push し、release.yml の完走と asset を確認する。tag が手元にだけある
# (前回の push が失敗した) 場合は、SHA を指していることを確かめて push からやり直す
release() {
  local existing run_id waited=0 local_tag
  if local_tag="$(git rev-parse --verify --quiet "refs/tags/$VERSION^{commit}")"; then
    [ "$local_tag" = "$SHA" ] ||
      fail "手元の tag $VERSION が $(short "$local_tag") を指しています (昇格対象は $(short "$SHA"))"
  else
    run git tag -a "$VERSION" -m "$VERSION" "$SHA"
  fi
  existing="$(gh run list --workflow="$RELEASE_WORKFLOW" --commit="$SHA" --event=push -L 10 \
    --json databaseId --jq '.[].databaseId' 2>/dev/null | tr '\n' ' ' || true)"
  run git push --no-verify origin "refs/tags/$VERSION" ||
    fail "tag $VERSION の push が失敗しました。main の ff は済んでいます。'fatal error in commit_refs_rules' なら docs/releasing.md「失敗したとき」の API 経由の手順で tag を作ってください。それ以外は原因を直して \`just ship-prod\` を再実行してください (手元の tag はそのまま使われます)"
  if [ -n "$DRY_RUN" ]; then
    log "(dry-run) release run の完走と asset $EXPECTED_ASSETS 個を確認します"
    return 0
  fi
  log "tag $VERSION を push しました"
  while [ "$waited" -lt "$RELEASE_RUN_DEADLINE" ]; do
    run_id="$(find_release_run "$existing")"
    [ -n "$run_id" ] && break
    sleep 5
    waited=$((waited + 5))
  done
  [ -n "${run_id:-}" ] ||
    fail "release の run が見つかりません。gh run list --workflow $RELEASE_WORKFLOW で確認してください (workflow が無効になっていないか: gh workflow list --all)"
  wait_run "$run_id" "release"
  [ "$RUN_CONCLUSION" != "cancelled" ] ||
    fail "release の run $run_id が cancelled です (main の ff と tag $VERSION の push は済んでいます)。\`gh run rerun $run_id\` で回し直し、完走後に \`just ship-prod\` を再実行すると Release を確かめ直します"
  [ "$RUN_CONCLUSION" = "success" ] ||
    fail "release の run $run_id が success ではありません: ${RUN_CONCLUSION:-不明}。gh run view $run_id --log-failed で確認してください。一部の job だけの失敗は \`gh run rerun $run_id --failed\` で再実行できます 完走後に \`just ship-prod\` を再実行すると Release を確かめ直します (対処は docs/releasing.md「失敗したとき」)"
  verify_release_assets
  log "リリース完了: $VERSION ($(short "$SHA"))  https://github.com/worgue/dsql-schema-migrate/releases/tag/$VERSION"
}

prod_phase() {
  resolve_prod_sha
  resolve_release
  prod_announce
  if [ "$FF_NEEDED" != 1 ] && [ "$RELEASE_ACTION" = "none" ]; then
    checkout_follow
    log "昇格するものはありません (origin/main は $(short "$SHA") を含み、$VERSION は tag 済みです)。Release を確かめ直します"
    verify_release_assets
    return 0
  fi
  if [ "$RELEASE_ACTION" = "tag" ]; then
    ship_confirm
  fi
  # ff するときだけ CI を検証する (main に入っている commit は昇格時に検証済み)
  if [ "$FF_NEEDED" = 1 ]; then
    verify_ci
  fi
  main_ff
  checkout_follow
  if [ "$RELEASE_ACTION" = "tag" ]; then
    release
  fi
  log "昇格完了: $(short "$SHA") (main$([ "$RELEASE_ACTION" = "tag" ] && echo " / リリース $VERSION"))"
}

case "$MODE" in
prod)
  require_main_worktree
  git fetch --quiet --tags origin
  require_current_ship
  prod_phase
  ;;
*)
  fail "未知のサブコマンドです: $MODE (prod)"
  ;;
esac
