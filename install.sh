#!/usr/bin/env bash
# Install the standalone agents binary from the private tomagranate/hangar
# repo. Needs the GitHub CLI, signed in: `gh auth login`.
set -euo pipefail

REPO="tomagranate/hangar"
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
  # Hangar also tags other packages. Pick the highest stable agents-vX.Y.Z tag.
  gh api "repos/$REPO/git/matching-refs/tags/agents-v" --jq '.[].ref' \
    | sed -n 's|^refs/tags/agents-v||p' \
    | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' \
    | sort -t. -k1,1n -k2,2n -k3,3n \
    | tail -1
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
  printf 'Downloading agents %s for %s...\n' "$version" "$target"
  gh release download "agents-v$version" --repo "$REPO" \
    --pattern "$asset" --pattern "$asset.sha256" --dir "$temporary"
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
  gh api "repos/$REPO/tarball/master" > "$temporary/master.tar.gz"
  tar -xzf "$temporary/master.tar.gz" -C "$temporary"
  local source
  source="$(find "$temporary" -maxdepth 2 -type d -path '*/agents' | head -1)"
  [[ -f "$source/Cargo.toml" ]] || fail "source archive is invalid"
  cargo build --locked --release --manifest-path "$source/Cargo.toml"
  mkdir -p "$INSTALL_DIR"
  install -m 755 "$source/target/release/agents" "$INSTALL_DIR/agents"
}

main() {
  need gh
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
