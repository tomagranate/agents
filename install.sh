#!/usr/bin/env bash
#
# Install agents CLI
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/tomagranate/agents/main/install.sh | sh
#
# Environment:
#   INSTALL_DIR   Binary dir (default: ~/.local/bin)
#   SHARE_DIR     Data/templates dir (default: ~/.local/share/agents)
#   VERSION       Tag without v, or "main" (default: latest release, else main)
#   SKIP_INIT     Set to 1 to skip agents init after install
#   FORCE_INIT    Set to 1 to pass --force to agents init
#
set -euo pipefail

REPO="tomagranate/agents"
GITHUB_URL="https://github.com/${REPO}"
RAW_URL="https://raw.githubusercontent.com/${REPO}"

BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'
CYAN='\033[0;36m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'

step() { echo -e "  ${CYAN}▸${RESET} $1"; }
success() { echo -e "  ${GREEN}✓${RESET} $1"; }
warn() { echo -e "  ${YELLOW}!${RESET} $1"; }
error() { echo -e "  ${RED}✗${RESET} $1" >&2; exit 1; }

print_header() {
  echo
  echo -e "${CYAN}${BOLD}"
  echo "  ╭─────────────────────────────────╮"
  echo "  │         agents installer        │"
  echo "  ╰─────────────────────────────────╯"
  echo -e "${RESET}"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || error "need '$1' on PATH"
}

resolve_version() {
  if [[ -n "${VERSION:-}" ]]; then
    echo "$VERSION"
    return
  fi
  local tag
  tag=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | sed -n 's/.*"tag_name": *"\(v\{0,1\}[^"]*\)".*/\1/p' | head -1 || true)
  if [[ -n "$tag" ]]; then
    echo "${tag#v}"
  else
    echo "main"
  fi
}

main() {
  print_header
  need_cmd curl
  need_cmd tar
  need_cmd mkdir

  local version ref archive_url tmp install_dir share_dir
  version="$(resolve_version)"
  install_dir="${INSTALL_DIR:-$HOME/.local/bin}"
  share_dir="${SHARE_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/agents}"

  if [[ "$version" == "main" || "$version" == "master" ]]; then
    ref="$version"
    archive_url="${GITHUB_URL}/archive/refs/heads/${ref}.tar.gz"
  else
    ref="v${version#v}"
    archive_url="${GITHUB_URL}/archive/refs/tags/${ref}.tar.gz"
  fi

  step "Installing agents ${version}"
  echo -e "    ${DIM}bin → ${install_dir}${RESET}"
  echo -e "    ${DIM}share → ${share_dir}${RESET}"
  echo -e "    ${DIM}source → ${archive_url}${RESET}"

  tmp="$(mktemp -d "${TMPDIR:-/tmp}/agents-install.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT

  step "Downloading"
  if ! curl -fsSL "$archive_url" -o "$tmp/src.tar.gz"; then
    if [[ "$version" != "main" ]]; then
      warn "tag download failed; falling back to main"
      archive_url="${GITHUB_URL}/archive/refs/heads/main.tar.gz"
      curl -fsSL "$archive_url" -o "$tmp/src.tar.gz" || error "download failed"
      ref="main"
    else
      error "download failed"
    fi
  fi

  step "Extracting"
  tar -xzf "$tmp/src.tar.gz" -C "$tmp"
  local root
  root="$(find "$tmp" -maxdepth 1 -type d -name 'agents-*' | head -1)"
  [[ -n "$root" && -f "$root/bin/agents" ]] || error "unexpected archive layout"

  step "Installing files"
  mkdir -p "$install_dir" "$share_dir/templates"
  install -m 755 "$root/bin/agents" "$install_dir/agents"
  # Sync version string if VERSION file present
  if [[ -f "$root/VERSION" ]]; then
    local ver
    ver="$(tr -d '[:space:]' <"$root/VERSION")"
    # rewrite embedded version if needed
    if ! grep -q "AGENTS_VERSION=\"$ver\"" "$install_dir/agents"; then
      sed -i.bak "s/^AGENTS_VERSION=.*/AGENTS_VERSION=\"$ver\"/" "$install_dir/agents" 2>/dev/null \
        || sed -i '' "s/^AGENTS_VERSION=.*/AGENTS_VERSION=\"$ver\"/" "$install_dir/agents"
      rm -f "$install_dir/agents.bak"
    fi
  fi
  rm -rf "$share_dir/templates"
  cp -R "$root/share/templates" "$share_dir/templates"
  success "installed $install_dir/agents"
  success "templates → $share_dir/templates"

  # PATH hint
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *)
      warn "Add to PATH (e.g. in ~/.zshrc):"
      echo -e "    ${DIM}export PATH=\"${install_dir}:\$PATH\"${RESET}"
      ;;
  esac

  export AGENTS_SHARE="$share_dir"
  export PATH="$install_dir:$PATH"

  if [[ "${SKIP_INIT:-0}" == "1" ]]; then
    warn "Skipping init (SKIP_INIT=1)"
  else
    step "Scaffolding config (agents init)"
    if [[ "${FORCE_INIT:-0}" == "1" ]]; then
      "$install_dir/agents" init --force || warn "init failed; run manually: agents init"
    else
      "$install_dir/agents" init || warn "init failed; run manually: agents init"
    fi
  fi

  echo
  success "agents is ready"
  echo -e "    ${DIM}$("$install_dir/agents" version 2>/dev/null || echo agents)${RESET}"
  echo
  echo "  Next:"
  echo "    agents status"
  echo "    agents skills"
  echo "    agents md"
  echo
  echo "  Edit shared rules:"
  echo "    \$EDITOR ~/.agents/AGENTS.md"
  echo
}

main "$@"
