#!/usr/bin/env bash
# x-backup 릴리스 자동화 — 빌드 → 패키징 → GitHub 릴리스 → Homebrew tap 갱신 → 검증.
#
# 한 번의 실행으로 4개 플랫폼(darwin/linux × arm64/amd64) 정적 바이너리를 빌드해
# v0.1.0과 동일한 규약(tarball 루트에 `x-backup` 0755 단일 엔트리 + checksums.txt)으로
# 패키징하고, GitHub 릴리스를 만든 뒤 Homebrew tap formula의 version/url/sha256을
# 갱신해 push한다. 마지막에 releases/latest와 자산 목록을 검증한다.
#
# 사용법:
#   scripts/release.sh                 # Cargo.toml 버전으로 릴리스
#   scripts/release.sh --version 0.3.0 # 버전 명시(앞의 v는 있어도 없어도 됨)
#   scripts/release.sh --dry-run       # 빌드·패키징까지만(게시/ tap 갱신 안 함)
#   scripts/release.sh --skip-tap      # Homebrew tap 갱신 생략
#
# 전제조건(스킬 SKILL.md 참고):
#   - 릴리스할 커밋에 vX.Y.Z 태그가 있고 origin에 push돼 있을 것(`--verify-tag`).
#   - cargo / cross / docker(실행 중) / gh(인증) / shasum / tar / ruby 사용 가능.
#   - tap(x-mesh/homebrew-tap) push 권한.
set -euo pipefail

REPO="x-mesh/x-backup"
TAP_REPO="x-mesh/homebrew-tap"
TAP_FORMULA="Formula/x-backup.rb"
BIN="x-backup"

# name|rust-triple|builder — checksums.txt 정렬 순서와 동일하게 유지.
TARGETS=(
  "darwin_amd64|x86_64-apple-darwin|cargo"
  "darwin_arm64|aarch64-apple-darwin|cargo"
  "linux_amd64|x86_64-unknown-linux-musl|cross"
  "linux_arm64|aarch64-unknown-linux-musl|cross"
)

VERSION=""
SKIP_TAP=0
DRY_RUN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2#v}"; shift 2 ;;
    --skip-tap) SKIP_TAP=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "알 수 없는 옵션: $1" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

[ -n "$VERSION" ] || VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
TAG="v$VERSION"
DIST="$ROOT/dist"

say()  { printf '\033[1;34m▶\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "필요한 도구 없음: $1"; }

say "릴리스 대상: $REPO $TAG  (dry-run=$DRY_RUN, skip-tap=$SKIP_TAP)"

# ── 0) 사전 점검 ─────────────────────────────────────────────────────────────
need cargo; need cross; need gh; need tar; need shasum; need git; need ruby; need rustup
docker info >/dev/null 2>&1 || die "docker 데몬 미실행(cross 빌드에 필요)"

# cross 0.2.5 환경 보정 — v0.3.0 릴리스에서 실제로 관측한 세 가지 실패를 막는다.
# (a) 기본 이미지가 Ubuntu 16.04(glibc 2.23)라 rustc 1.98 빌드 스크립트가
#     `libc.so.6: version GLIBC_2.28 not found`로 죽는다 → main 태그(Ubuntu 24.04).
#     main은 움직이는 태그다. 고정이 필요하면 이 env를 미리 설정해 덮어쓴다.
: "${CROSS_TARGET_X86_64_UNKNOWN_LINUX_MUSL_IMAGE:=ghcr.io/cross-rs/x86_64-unknown-linux-musl:main}"
: "${CROSS_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_IMAGE:=ghcr.io/cross-rs/aarch64-unknown-linux-musl:main}"
export CROSS_TARGET_X86_64_UNKNOWN_LINUX_MUSL_IMAGE CROSS_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_IMAGE

# (b) ghcr.io/cross-rs 이미지는 amd64 전용이라 arm64 호스트에서 docker가
#     `no matching manifest for linux/arm64/v8`로 거부한다 → 플랫폼 고정(에뮬레이션).
case "$(uname -m)" in
  arm64|aarch64)
    : "${DOCKER_DEFAULT_PLATFORM:=linux/amd64}"; export DOCKER_DEFAULT_PLATFORM
    say "arm64 호스트 — cross 컨테이너를 $DOCKER_DEFAULT_PLATFORM 에뮬레이션으로 실행합니다(빌드가 느립니다)."
    ;;
esac

# (c) cross는 호스트 rustup의 x86_64-unknown-linux-gnu 툴체인을 컨테이너에 마운트한다.
#     Apple Silicon rustup은 non-host 툴체인 설치를 거부하므로 미리 깔려 있어야 한다.
rustup toolchain list 2>/dev/null | grep -q 'x86_64-unknown-linux-gnu' || \
  die "cross가 마운트할 툴체인 없음 — 먼저: rustup toolchain install stable-x86_64-unknown-linux-gnu --force-non-host --profile minimal"

# clean 트리·원격 태그는 실제 게시에만 필요(dry-run은 빌드·패키징만 검증).
if [ "$DRY_RUN" = 0 ]; then
  gh auth status >/dev/null 2>&1 || die "gh 인증 필요: gh auth login"
  git diff --quiet && git diff --cached --quiet || die "워킹트리가 clean이 아닙니다 — 커밋 후 릴리스하세요"
  git rev-parse "$TAG" >/dev/null 2>&1 || \
    die "로컬 태그 $TAG 없음 — 먼저: git tag $TAG && git push origin $TAG"
  if [ "$(git rev-list -n1 "$TAG")" != "$(git rev-parse HEAD)" ]; then
    say "주의: HEAD가 $TAG 커밋과 다릅니다 — 릴리스는 $TAG 커밋 기준입니다."
  fi
fi

# ── 1) 빌드 + 패키징 ─────────────────────────────────────────────────────────
rm -rf "$DIST"; mkdir -p "$DIST"
for entry in "${TARGETS[@]}"; do
  IFS='|' read -r name triple builder <<<"$entry"
  say "build $name ($triple) via $builder"
  if [ "$builder" = cross ]; then
    cross build --release --target "$triple"
  else
    rustup target list --installed 2>/dev/null | grep -qx "$triple" || rustup target add "$triple"
    cargo build --release --target "$triple"
  fi
  bin="target/$triple/release/$BIN"
  [ -f "$bin" ] || die "산출물 없음: $bin"
  stage="$(mktemp -d)"
  cp "$bin" "$stage/$BIN"; chmod 0755 "$stage/$BIN"
  # 결정적 tarball: mtime 고정 + gzip -n(타임스탬프 제거) → 같은 바이너리면 같은 해시.
  # COPYFILE_DISABLE: macOS tar의 AppleDouble(._*)/PaxHeader 혼입 방지.
  touch -t 200001010000 "$stage/$BIN"
  COPYFILE_DISABLE=1 tar -C "$stage" -cf - "$BIN" | gzip -n > "$DIST/${BIN}_${name}.tar.gz"
  rm -rf "$stage"
  ok "packaged: dist/${BIN}_${name}.tar.gz"
done

( cd "$DIST" && shasum -a 256 \
    "${BIN}_darwin_amd64.tar.gz" "${BIN}_darwin_arm64.tar.gz" \
    "${BIN}_linux_amd64.tar.gz"  "${BIN}_linux_arm64.tar.gz" > checksums.txt )

# 빌드된 darwin 바이너리 버전이 VERSION과 일치하는지(태그/Cargo.toml 정합성).
host_bin="target/aarch64-apple-darwin/release/$BIN"
[ -x "$host_bin" ] || host_bin="target/x86_64-apple-darwin/release/$BIN"
got="$("$host_bin" --version 2>/dev/null | awk '{print $2}')"
[ "$got" = "$VERSION" ] || die "빌드 산출물 버전($got) != $VERSION — Cargo.toml/태그 불일치"
ok "빌드 버전 일치: $VERSION"
say "checksums.txt:"; sed 's/^/    /' "$DIST/checksums.txt"

if [ "$DRY_RUN" = 1 ]; then
  ok "dry-run 완료 — 자산은 $DIST 에 있습니다(게시/ tap 갱신 생략)."
  exit 0
fi

# ── 2) GitHub 릴리스 ─────────────────────────────────────────────────────────
NOTES="$(mktemp)"
awk -v v="$VERSION" '
  $0 ~ "^## \\[" v "\\]" {f=1; print; next}
  /^## \[/ {f=0}
  f {print}
' CHANGELOG.md > "$NOTES" || true
[ -s "$NOTES" ] || printf 'x-backup %s\n' "$TAG" > "$NOTES"

assets=("$DIST/${BIN}_darwin_amd64.tar.gz" "$DIST/${BIN}_darwin_arm64.tar.gz"
        "$DIST/${BIN}_linux_amd64.tar.gz"  "$DIST/${BIN}_linux_arm64.tar.gz"
        "$DIST/checksums.txt")
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  say "릴리스 $TAG 이미 존재 — 자산 갱신(--clobber)"
  gh release upload "$TAG" --repo "$REPO" --clobber "${assets[@]}"
else
  say "릴리스 생성: $TAG"
  gh release create "$TAG" --repo "$REPO" --verify-tag \
    --title "$TAG" --notes-file "$NOTES" "${assets[@]}"
fi
ok "GitHub 릴리스 게시 완료"

# ── 3) Homebrew tap 갱신 ─────────────────────────────────────────────────────
if [ "$SKIP_TAP" = 0 ]; then
  say "Homebrew tap 갱신: $TAP_REPO/$TAP_FORMULA"
  H_damd=$(awk '$2 ~ /darwin_amd64/{print $1}' "$DIST/checksums.txt")
  H_darm=$(awk '$2 ~ /darwin_arm64/{print $1}' "$DIST/checksums.txt")
  H_lamd=$(awk '$2 ~ /linux_amd64/{print $1}'  "$DIST/checksums.txt")
  H_larm=$(awk '$2 ~ /linux_arm64/{print $1}'  "$DIST/checksums.txt")
  tapdir="$(mktemp -d)"
  gh repo clone "$TAP_REPO" "$tapdir/tap" -- --depth 1 >/dev/null 2>&1
  f="$tapdir/tap/$TAP_FORMULA"
  [ -f "$f" ] || die "tap formula 없음: $TAP_FORMULA"
  # version 줄 + 모든 download URL의 vX.Y.Z 교체(BSD/GNU sed 공통: -i.bak).
  sed -i.bak -E \
    -e "s|^([[:space:]]*version )\"[^\"]*\"|\\1\"$VERSION\"|" \
    -e "s|releases/download/v[0-9][0-9.]*|releases/download/$TAG|g" "$f"
  rm -f "$f.bak"
  # sha256은 직전 url 줄의 플랫폼명으로 식별해 교체.
  awk -v damd="$H_damd" -v darm="$H_darm" -v lamd="$H_lamd" -v larm="$H_larm" '
    /url .*darwin_amd64/ {c="damd"}
    /url .*darwin_arm64/ {c="darm"}
    /url .*linux_amd64/  {c="lamd"}
    /url .*linux_arm64/  {c="larm"}
    /^[[:space:]]*sha256 / {
      h=(c=="damd"?damd:(c=="darm"?darm:(c=="lamd"?lamd:larm)))
      sub(/"[0-9a-fA-F]*"/, "\"" h "\"")
    }
    { print }
  ' "$f" > "$f.new" && mv "$f.new" "$f"
  ruby -c "$f" >/dev/null || die "formula 문법 오류 — push 중단"
  # 4개 sha256이 정확히 반영됐는지 교차검증.
  for h in "$H_damd" "$H_darm" "$H_lamd" "$H_larm"; do
    grep -q "$h" "$f" || die "formula에 sha256 $h 미반영 — push 중단"
  done
  ( cd "$tapdir/tap"
    git add "$TAP_FORMULA"
    if git diff --cached --quiet; then
      say "tap formula 변경 없음(이미 $VERSION) — push 생략"
    else
      git commit -m "$BIN $VERSION" >/dev/null
      git push >/dev/null
      ok "tap formula push 완료: $BIN $VERSION"
    fi )
  rm -rf "$tapdir"
fi

# ── 4) 검증 ──────────────────────────────────────────────────────────────────
say "검증"
latest="$(gh api "repos/$REPO/releases/latest" --jq .tag_name 2>/dev/null || echo '?')"
if [ "$latest" = "$TAG" ]; then ok "releases/latest = $TAG"
else say "releases/latest = $latest (prerelease거나 latest 미설정일 수 있음)"; fi
gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name' | sed 's/^/    asset: /'
ok "완료. 설치측 검증: x-backup update  /  brew update && brew upgrade $TAP_REPO/$BIN"
say "참고: dist/ 는 산출물입니다(.gitignore 처리됨). 정리하려면: rm -rf dist"
