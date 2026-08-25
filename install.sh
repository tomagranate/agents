#!/usr/bin/env bash
# Install the standalone agents binary.
set -euo pipefail

REPO="tomagranate/agents"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
INSTALL_TEMPORARY=""

cleanup() {
  if [[ -n "$INSTALL_TEMPORARY" && -d "$INSTALL_TEMPORARY" ]]; then
    rm -rf -- "$INSTALL_TEMPORARY"
  fi
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "need '$1' on PATH"
}

resolve_version() {
  if [[ -n "${VERSION:-}" ]]; then
    printf '%s' "${VERSION#v}"
    return
  fi
  curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' \
    | head -1
}

release_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64) printf 'aarch64-apple-darwin' ;;
    Darwin/x86_64) printf 'x86_64-apple-darwin' ;;
    Linux/x86_64) printf 'x86_64-unknown-linux-gnu' ;;
    Linux/aarch64|Linux/arm64) printf 'aarch64-unknown-linux-gnu' ;;
    *) fail "no release build for $os/$arch" ;;
  esac
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

install_release() {
  local version="$1" target="$2" temporary="$3"
  local asset="agents-$target.tar.gz"
  local base="https://github.com/$REPO/releases/download/v$version"
  printf 'Downloading agents %s for %s...\n' "$version" "$target"
  curl -fsSL "$base/$asset" -o "$temporary/$asset"
  curl -fsSL "$base/$asset.sha256" -o "$temporary/$asset.sha256"
  local expected actual
  expected="$(awk '{print $1}' "$temporary/$asset.sha256")"
  actual="$(sha256_file "$temporary/$asset")"
  [[ -n "$expected" && "$actual" == "$expected" ]] || fail "release checksum mismatch"
  tar -xzf "$temporary/$asset" -C "$temporary"
  [[ -x "$temporary/agents" ]] || fail "release archive has no agents binary"
  mkdir -p "$INSTALL_DIR"
  install -m 755 "$temporary/agents" "$INSTALL_DIR/agents"
}

install_master() {
  local temporary="$1"
  need cargo
  printf 'Building agents from master...\n'
  curl -fsSL "https://github.com/$REPO/archive/refs/heads/master.tar.gz" -o "$temporary/master.tar.gz"
  tar -xzf "$temporary/master.tar.gz" -C "$temporary"
  local source
  source="$(find "$temporary" -maxdepth 1 -type d -name 'agents-*' | head -1)"
  [[ -f "$source/Cargo.toml" ]] || fail "source archive is invalid"
  cargo build --locked --release --manifest-path "$source/Cargo.toml"
  mkdir -p "$INSTALL_DIR"
  install -m 755 "$source/target/release/agents" "$INSTALL_DIR/agents"
}

main() {
  need curl
  need tar
  need install
  local version target temporary
  version="$(resolve_version)"
  [[ -n "$version" ]] || fail "could not find a release"
  target="$(release_target)"
  temporary="$(mktemp -d "${TMPDIR:-/tmp}/agents-install.XXXXXX")"
  INSTALL_TEMPORARY="$temporary"
  trap cleanup EXIT

  if [[ "$version" == "main" || "$version" == "master" ]]; then
    install_master "$temporary"
  else
    install_release "$version" "$target" "$temporary"
  fi

  printf 'Installed %s\n' "$INSTALL_DIR/agents"
  "$INSTALL_DIR/agents" version
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) printf 'Add %s to PATH.\n' "$INSTALL_DIR" ;;
  esac
  if [[ "${SKIP_INIT:-0}" != "1" && ! -f "$HOME/.agents/AGENTS.md" ]]; then
    "$INSTALL_DIR/agents" init
  fi
}

main "$@"
