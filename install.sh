#!/bin/sh
# x-backup installer — POSIX sh (gk install.sh 컨벤션)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/x-mesh/x-backup/main/install.sh | sh
#
# 저장소 체크아웃에서 직접: ./install.sh
#
# Env overrides:
#   XB_VERSION=v0.1.0        특정 버전 고정 (기본: latest)
#   XB_INSTALL_DIR=/path     설치 위치 (기본: ~/.local/bin)

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

# --- pick version -----------------------------------------------------------
version=${XB_VERSION:-}
if [ -z "$version" ]; then
  version=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$version" ] || err "최신 릴리스 태그를 확인할 수 없습니다 — XB_VERSION으로 직접 지정하세요"
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

asset="${BIN}_${os}_${arch}.tar.gz"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t xb-install)
trap 'rm -rf "$tmp"' EXIT INT TERM

# --- download ----------------------------------------------------------------
base="https://github.com/${REPO}/releases/download/${version}"
info "downloading: ${asset} ${version}"
curl -fsSL "${base}/${asset}"      -o "$tmp/$asset"        || err "다운로드 실패: ${base}/${asset}"
curl -fsSL "${base}/checksums.txt" -o "$tmp/checksums.txt" || err "다운로드 실패: checksums.txt"

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
