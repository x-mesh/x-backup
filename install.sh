#!/bin/sh
# x-backup installer — POSIX sh (gk install.sh 컨벤션)
#
# Usage (public 전환 후):
#   curl -fsSL https://raw.githubusercontent.com/x-mesh/x-backup/main/install.sh | sh
#
# Usage (private 단계 — 토큰 필요):
#   GITHUB_TOKEN=$(gh auth token) sh -c \
#     "$(gh api repos/x-mesh/x-backup/contents/install.sh --jq .content | base64 -d)"
#   또는 저장소 체크아웃에서: ./install.sh
#
# Env overrides:
#   XB_VERSION=v0.1.0        특정 버전 고정 (기본: latest)
#   XB_INSTALL_DIR=/path     설치 위치 (기본: ~/.local/bin)
#   GITHUB_TOKEN / GH_TOKEN  private 저장소 자산 다운로드용 (gh CLI가 있으면 자동)

set -eu

REPO="x-mesh/x-backup"
BIN="x-backup"

err() { printf "x-backup-install: %s\n" "$*" >&2; exit 1; }
info() { printf "x-backup-install: %s\n" "$*"; }

# --- detect os/arch ---------------------------------------------------------
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in
  x86_64|amd64)  arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) err "지원하지 않는 아키텍처: $arch" ;;
esac
case "$os" in
  linux|darwin) ;;
  *) err "지원하지 않는 OS: $os" ;;
esac

command -v curl >/dev/null 2>&1 || err "curl이 필요합니다"
command -v tar  >/dev/null 2>&1 || err "tar가 필요합니다"

# --- token (private 단계) ----------------------------------------------------
token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
if [ -z "$token" ] && command -v gh >/dev/null 2>&1; then
  token=$(gh auth token 2>/dev/null || true)
fi

auth_curl() {
  if [ -n "$token" ]; then
    curl -fsSL -H "Authorization: Bearer $token" "$@"
  else
    curl -fsSL "$@"
  fi
}

# --- pick version -----------------------------------------------------------
version=${XB_VERSION:-}
if [ -z "$version" ]; then
  version=$(auth_curl "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$version" ] || err "최신 릴리스 태그를 확인할 수 없습니다 — private면 GITHUB_TOKEN을 설정하세요"
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

asset="${BIN}_${os}_${arch}.tar.gz"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t xb-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

# --- download ----------------------------------------------------------------
# 1순위 gh(있으면 private 자산을 가장 안정적으로 받는다), 2순위 토큰 + API asset
# endpoint, 3순위 public browser URL(저장소 공개 후 토큰 없이 동작하는 경로).
download() {
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    info "downloading via gh: ${asset} ${version}"
    gh release download "$version" -R "$REPO" -p "$asset" -p "checksums.txt" -D "$tmp" \
      || err "gh release download 실패"
    return
  fi
  if [ -n "$token" ]; then
    info "downloading via API: ${asset} ${version}"
    rel_json=$(auth_curl "https://api.github.com/repos/${REPO}/releases/tags/${version}") \
      || err "릴리스 조회 실패: ${version}"
    if command -v python3 >/dev/null 2>&1; then
      printf '%s' "$rel_json" | python3 -c "
import json, sys
rel = json.load(sys.stdin)
for a in rel['assets']:
    if a['name'] in ('$asset', 'checksums.txt'):
        print(a['name'], a['url'])
" | while read -r name url; do
        curl -fsSL -H "Authorization: Bearer $token" -H "Accept: application/octet-stream" \
          -o "$tmp/$name" "$url" || exit 1
      done || err "자산 다운로드 실패"
      [ -s "$tmp/$asset" ] || err "자산 다운로드 실패: $asset"
      return
    fi
    err "private 자산 파싱에 gh 또는 python3가 필요합니다"
  fi
  # public 경로(저장소 공개 후).
  base="https://github.com/${REPO}/releases/download/${version}"
  info "downloading: ${asset} ${version}"
  curl -fsSL "${base}/${asset}"      -o "$tmp/$asset"        || err "다운로드 실패(private면 GITHUB_TOKEN 필요): ${base}/${asset}"
  curl -fsSL "${base}/checksums.txt" -o "$tmp/checksums.txt" || err "다운로드 실패: checksums.txt"
}
download

# --- verify -------------------------------------------------------------------
expected=$(awk -v f="$asset" '$2 == f {print $1}' "$tmp/checksums.txt")
[ -n "$expected" ] || err "checksums.txt에 $asset 항목이 없습니다"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
else
  err "sha256 도구가 없습니다 (coreutils 또는 shasum 설치)"
fi
[ "$expected" = "$actual" ] || err "sha256 불일치 (기대 $expected, 실제 $actual)"

tar -xzf "$tmp/$asset" -C "$tmp" "$BIN" || err "압축 해제 실패"

# --- install -------------------------------------------------------------------
target_dir=${XB_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$target_dir"
install -m 0755 "$tmp/$BIN" "$target_dir/$BIN"
info "installed: $target_dir/$BIN ($version)"

case ":$PATH:" in
  *":$target_dir:"*) ;;
  *) info "주의: $target_dir 가 PATH에 없습니다 — 셸 rc에 추가하세요: export PATH=\"$target_dir:\$PATH\"" ;;
esac

"$target_dir/$BIN" --version 2>/dev/null || true
info "갱신은 언제든: $BIN update"
