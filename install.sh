#!/usr/bin/env bash
# ============================================================================
# claudebase installer
# ============================================================================
#
# Installs the `claudebase` CLI binary and the associated agent toolkit
# (rules, commands, agents) into `~/.claude/`. Designed to be invoked
# either standalone (one-shot from anywhere) or chained from the
# `claude-code-sdlc` installer (which curls this script and pipes to bash).
#
# Usage:
#   bash install.sh                  Install user-level binary + prompts
#   bash install.sh --yes            Skip confirmation prompts
#   bash install.sh --local          Use local checkout (skip git clone)
#   bash install.sh --help           Show help
#
# Pipe form (used by the SDLC installer):
#   curl -fsSL https://raw.githubusercontent.com/codefather-labs/claudebase/main/install.sh | bash -s -- --yes
# ============================================================================

set -u

# ============================================================================
# Constants
# ============================================================================
CLAUDEBASE_VERSION="0.5.0"
CLAUDEBASE_PDFIUM_VERSION="chromium/7802"
REPO_URL="https://github.com/codefather-labs/claudebase.git"
RELEASE_BASE="https://github.com/codefather-labs/claudebase/releases/download"

CLAUDE_DIR="$HOME/.claude"
SCRIPT_DIR=""
LOCAL_MODE=false
ASSUME_YES=false

# ============================================================================
# Logging
# ============================================================================
if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'
  BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

log_info()  { echo -e "${BLUE}[INFO]${NC} $1"; }
log_ok()    { echo -e "${GREEN}  [OK]${NC}  $1"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

print_help() {
  cat <<HELPEOF
${BOLD}claudebase installer${NC}

Installs the claudebase CLI binary and agent toolkit (rules, commands,
agents) into ~/.claude/.

USAGE:
  bash install.sh [OPTIONS]

OPTIONS:
  --yes                       Skip confirmation prompts
  --local                     Use local checkout (skip git clone)
  --help                      Show this help

WHAT GETS INSTALLED:
  ~/.claude/tools/claudebase/claudebase     CLI binary (downloaded from releases)
  ~/.claude/tools/claudebase/pdfium/        PDFium dynamic library for PDF extraction
  ~/.claude/tools/claudebase/models/        e5-multilingual-small encoder (pre-cached)
  ~/.claude/rules/knowledge-base.md         CLI contract + citation discipline
  ~/.claude/rules/knowledge-base-tool.md    Usage mandate + insights protocol
  ~/.claude/rules/tool-limitations.md       Read/grep/bash truncation gotchas
  ~/.claude/commands/knowledge-ingest.md    /knowledge-ingest skill
  ~/.claude/commands/reflect.md             /reflect skill (DMN observation)
  ~/.claude/commands/consolidate.md         /consolidate skill (drift detection)
  ~/.claude/agents/reflection.md            reflection agent (Drift persona)
  ~/.claude/agents/consolidator.md          consolidator agent (Mnem persona)
  /usr/local/bin/claudebase                 Global alias (symlink)
  ~/.claude/settings.json                   Bash allowlist entry merged
HELPEOF
}

# ============================================================================
# Argument parsing
# ============================================================================
while [ $# -gt 0 ]; do
  case "$1" in
    --yes|-y)    ASSUME_YES=true; shift ;;
    --local)     LOCAL_MODE=true; shift ;;
    --help|-h)   print_help; exit 0 ;;
    *) log_error "unknown flag: $1"; print_help; exit 2 ;;
  esac
done

confirm() {
  if [ "$ASSUME_YES" = true ]; then return 0; fi
  read -r -p "$1 [y/N] " ans
  case "$ans" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# ============================================================================
# Source-dir resolution
# ============================================================================
get_source_dir() {
  if [ "$LOCAL_MODE" = true ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [ ! -d "$SCRIPT_DIR/rules" ] || [ ! -d "$SCRIPT_DIR/commands" ] || [ ! -d "$SCRIPT_DIR/agents" ]; then
      log_error "--local requires running from a claudebase checkout root (with rules/ commands/ agents/)"
      exit 1
    fi
  else
    SCRIPT_DIR=$(mktemp -d)
    log_info "Cloning claudebase from $REPO_URL..."
    if ! git clone --depth 1 --quiet "$REPO_URL" "$SCRIPT_DIR" 2>/dev/null; then
      log_error "Failed to clone $REPO_URL. Check your internet connection."
      rm -rf "$SCRIPT_DIR"
      exit 1
    fi
    log_ok "Repository cloned"
  fi
}

# ============================================================================
# Install prompts/rules/commands/agents into ~/.claude/
# ============================================================================
install_prompts() {
  mkdir -p "$CLAUDE_DIR/rules" "$CLAUDE_DIR/commands" "$CLAUDE_DIR/agents"

  for f in "$SCRIPT_DIR"/rules/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/rules/"
    log_ok "rules/$(basename "$f")"
  done

  for f in "$SCRIPT_DIR"/commands/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/commands/"
    log_ok "commands/$(basename "$f")"
  done

  for f in "$SCRIPT_DIR"/agents/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/agents/"
    log_ok "agents/$(basename "$f")"
  done
}

# ============================================================================
# Download claudebase binary from GitHub releases
# ============================================================================
install_binary() {
  local target_dir="$CLAUDE_DIR/tools/claudebase"
  mkdir -p "$target_dir"

  local platform exe_ext=""
  case "$(uname -ms)" in
    "Darwin arm64")  platform="darwin-arm64"  ;;
    "Darwin x86_64") platform="darwin-x64"    ;;
    "Linux x86_64")  platform="linux-x64"     ;;
    "Linux aarch64") platform="linux-arm64"   ;;
    MINGW*|MSYS*|CYGWIN*)
      case "$(uname -m)" in
        x86_64) platform="windows-x64"; exe_ext=".exe" ;;
        *)
          log_warn "unsupported Windows arch: $(uname -m); skipping binary"
          return 0
          ;;
      esac
      ;;
    *)
      log_warn "binary unavailable for $(uname -ms); install cargo or build from source"
      return 0
      ;;
  esac

  local target_bin="$target_dir/claudebase${exe_ext}"

  if [ -x "$target_bin" ]; then
    local existing_ver
    existing_ver="$("$target_bin" --version 2>/dev/null | awk '{print $2}' || true)"
    if [ "$existing_ver" = "$CLAUDEBASE_VERSION" ]; then
      log_ok "claudebase binary already at version $CLAUDEBASE_VERSION"
      return 0
    fi
  fi

  local url="${RELEASE_BASE}/claudebase-v${CLAUDEBASE_VERSION}/claudebase-${platform}${exe_ext}"
  local tmp; tmp="$(mktemp)"

  if command -v curl >/dev/null 2>&1; then
    if ! curl --proto '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 120 "$url" -o "$tmp"; then
      rm -f "$tmp"
      log_warn "claudebase binary download failed (curl). Build from source: cargo install --git $REPO_URL"
      return 0
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget --https-only --secure-protocol=TLSv1_2 --max-redirect=5 --timeout=120 -q -O "$tmp" "$url"; then
      rm -f "$tmp"
      log_warn "claudebase binary download failed (wget). Build from source: cargo install --git $REPO_URL"
      return 0
    fi
  else
    rm -f "$tmp"
    log_warn "neither curl nor wget available; cannot install binary"
    return 0
  fi

  chmod +x "$tmp"
  if ! "$tmp" --version >/dev/null 2>&1; then
    log_warn "downloaded binary failed --version smoke; not installing"
    rm -f "$tmp"
    return 0
  fi

  mv "$tmp" "$target_bin"
  chmod +x "$target_bin"
  log_ok "tools/claudebase/claudebase ($platform)"
}

# ============================================================================
# Register global `claudebase` alias (symlink into first writable PATH dir)
# ============================================================================
register_alias() {
  local exe_ext=""
  case "$(uname -ms)" in MINGW*|MSYS*|CYGWIN*) exe_ext=".exe" ;; esac
  local target_bin="$CLAUDE_DIR/tools/claudebase/claudebase${exe_ext}"

  if [ ! -x "$target_bin" ]; then
    log_warn "alias: target binary not found at $target_bin; skipping"
    return 0
  fi

  local link_dir=""
  for dir in "/usr/local/bin" "/opt/homebrew/bin" "$HOME/.local/bin"; do
    if [ -d "$dir" ] && [ -w "$dir" ]; then link_dir="$dir"; break; fi
  done
  if [ -z "$link_dir" ]; then
    if mkdir -p "$HOME/.local/bin" 2>/dev/null && [ -w "$HOME/.local/bin" ]; then
      link_dir="$HOME/.local/bin"
    fi
  fi
  if [ -z "$link_dir" ]; then
    log_warn "alias: no writable PATH directory found"
    log_warn "  manual setup: ln -sf $target_bin /usr/local/bin/claudebase"
    return 0
  fi

  local link_path="$link_dir/claudebase"
  if [ -e "$link_path" ] && [ ! -L "$link_path" ]; then
    log_warn "alias: $link_path is a regular file; refusing to overwrite"
    return 0
  fi
  if [ -L "$link_path" ] && [ "$(readlink "$link_path")" = "$target_bin" ]; then
    log_ok "claudebase alias already in place ($link_path)"
    return 0
  fi
  rm -f "$link_path"
  ln -s "$target_bin" "$link_path"
  log_ok "claudebase alias: $link_path -> $target_bin"

  case ":$PATH:" in
    *":$link_dir:"*) ;;
    *)
      log_warn "  NOTE: $link_dir is not on PATH for the current shell"
      log_warn "  add to your shell rc: export PATH=\"$link_dir:\$PATH\""
      ;;
  esac
}

# ============================================================================
# Bash allowlist merge (settings.json)
# ============================================================================
register_bash_allowlist() {
  local settings="$CLAUDE_DIR/settings.json"
  local entry='~/.claude/tools/claudebase/claudebase *'

  if [ ! -f "$settings" ]; then
    mkdir -p "$CLAUDE_DIR"
    cat > "$settings" <<EOF
{"permissions":{"allow":["$entry"]}}
EOF
    chmod 0644 "$settings"
    log_ok "settings.json (created with claudebase allowlist)"
    return 0
  fi

  if command -v jq >/dev/null 2>&1; then
    local tmp; tmp="$(mktemp)"
    if jq --arg new "$entry" \
         '(.permissions //= {}) | (.permissions.allow //= []) | .permissions.allow = ((.permissions.allow + [$new]) | unique)' \
         "$settings" > "$tmp" \
       && jq -e '.' "$tmp" >/dev/null 2>&1; then
      mv "$tmp" "$settings"
      chmod 0644 "$settings"
      log_ok "settings.json (claudebase allowlist merged)"
    else
      rm -f "$tmp"
      log_warn "settings.json merge failed; add manually: $entry"
    fi
  else
    if grep -Fq "$entry" "$settings"; then
      log_ok "settings.json already contains claudebase allowlist"
    else
      log_warn "jq required for safe settings.json merge — install jq or add manually: $entry"
    fi
  fi
}

# ============================================================================
# Install pdfium native library
# ============================================================================
install_pdfium() {
  (
    set +e
    umask 0022

    local target_dir="$CLAUDE_DIR/tools/claudebase/pdfium"
    local lib_dir="$target_dir/lib"
    local sentinel="$target_dir/.version"

    if [ -f "$sentinel" ]; then
      local existing; existing=$(cat "$sentinel" 2>/dev/null)
      if [ "$existing" = "$CLAUDEBASE_PDFIUM_VERSION" ]; then
        log_ok "pdfium already at version $CLAUDEBASE_PDFIUM_VERSION"
        return 0
      fi
    fi

    local platform asset
    case "$(uname -s)/$(uname -m)" in
      Darwin/arm64)   platform=darwin-arm64;  asset=pdfium-mac-arm64.tgz   ;;
      Darwin/x86_64)  platform=darwin-x64;    asset=pdfium-mac-x64.tgz     ;;
      Linux/x86_64)   platform=linux-x64;     asset=pdfium-linux-x64.tgz   ;;
      Linux/aarch64)  platform=linux-arm64;   asset=pdfium-linux-arm64.tgz ;;
      *)
        log_warn "pdfium unavailable for $(uname -s)/$(uname -m); PDF extraction will fail at runtime"
        return 0
        ;;
    esac

    local url="https://github.com/bblanchon/pdfium-binaries/releases/download/${CLAUDEBASE_PDFIUM_VERSION}/${asset}"
    local tmp_archive staging
    tmp_archive=$(mktemp -t pdfium.XXXXXX) || { log_warn "mktemp failed"; return 0; }
    staging=$(mktemp -d -t pdfium.XXXXXX) || { log_warn "mktemp -d failed"; rm -f "$tmp_archive"; return 0; }
    trap 'rm -f "$tmp_archive"; rm -rf "$staging" 2>/dev/null' EXIT

    if command -v curl >/dev/null 2>&1; then
      if ! curl --proto '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 120 "$url" -o "$tmp_archive"; then
        log_warn "pdfium download failed (curl); skipping PDF support"; return 0
      fi
    elif command -v wget >/dev/null 2>&1; then
      if ! wget --https-only --secure-protocol=TLSv1_2 --max-redirect=5 --timeout=120 -q -O "$tmp_archive" "$url"; then
        log_warn "pdfium download failed (wget); skipping PDF support"; return 0
      fi
    else
      log_warn "neither curl nor wget available; skipping pdfium"; return 0
    fi

    if tar -tzf "$tmp_archive" 2>/dev/null | grep -E '^/|(^|/)\.\.(/|$)' >/dev/null; then
      log_warn "pdfium archive contains traversal entries; refusing"; return 0
    fi

    if ! tar --no-same-owner --no-same-permissions -xzf "$tmp_archive" -C "$staging" 2>/dev/null; then
      log_warn "pdfium extraction failed"; return 0
    fi

    if find "$staging" -path '*..*' -print -quit 2>/dev/null | grep -q .; then
      log_warn "pdfium produced traversal paths post-extract; refusing"; return 0
    fi

    if find "$staging" -perm /6000 -print -quit 2>/dev/null | grep -q .; then
      log_warn "pdfium contains setuid/setgid files; refusing"; return 0
    fi

    local extracted_lib
    extracted_lib=$(find "$staging" -maxdepth 3 -name "libpdfium*" -type f -print -quit 2>/dev/null)
    if [ -z "$extracted_lib" ]; then
      log_warn "no libpdfium found in extracted archive"; return 0
    fi

    mkdir -p "$lib_dir"
    cp "$extracted_lib" "$lib_dir/"
    chmod 0755 "$lib_dir"/libpdfium*
    echo "$CLAUDEBASE_PDFIUM_VERSION" > "$sentinel"
    chmod 0644 "$sentinel"

    if ! [ -s "$lib_dir/libpdfium.dylib" ] && ! [ -s "$lib_dir/libpdfium.so" ]; then
      log_warn "pdfium post-install integrity check failed"
      rm -rf "$target_dir"
      return 0
    fi

    log_ok "pdfium installed: ${platform} (version ${CLAUDEBASE_PDFIUM_VERSION})"
    return 0
  )
  return 0
}

# ============================================================================
# Register the claudebase plugin with Claude Code (marketplace + install)
# ============================================================================
# Idempotent — no-op when:
#   - `claude` CLI is not on PATH (skip with INFO; user can run by hand)
#   - marketplace 'claudebase-dev' is already registered
#   - plugin 'claudebase@claudebase-dev' is already installed
#
# This mirrors the official Anthropic telegram plugin's install UX:
# `bash install.sh` ends with the plugin ready to use after a single
# `claude --channels plugin:claudebase@claudebase-dev` launch.
#
# The marketplace source is the public github repo (codefather-labs/
# claudebase). install.sh used to be the only way users got the plugin;
# now the github marketplace path is the canonical install method and
# install.sh just bootstraps it.
register_claude_plugin() {
  if ! command -v claude >/dev/null 2>&1; then
    log_info "claude CLI not on PATH; skipping plugin registration"
    log_info "  to install manually later:"
    log_info "    claude plugin marketplace add codefather-labs/claudebase"
    log_info "    claude plugin install claudebase@claudebase-dev"
    return 0
  fi

  # marketplace add — idempotent at the claude CLI level (already-present
  # returns success without re-cloning).
  log_info "Registering claudebase-dev marketplace (github: codefather-labs/claudebase)..."
  if claude plugin marketplace add codefather-labs/claudebase 2>&1 | grep -qE "already|registered|added"; then
    log_ok "marketplace registered (or already present)"
  else
    # Even on warning output, claude returns 0 if marketplace works. Don't bail.
    log_ok "marketplace add invoked"
  fi

  # Install the plugin. `claude plugin install` is idempotent — re-run is
  # safe and refreshes from the marketplace source.
  log_info "Installing claudebase@claudebase-dev plugin..."
  if claude plugin install claudebase@claudebase-dev 2>&1 | tail -3; then
    log_ok "plugin installed"
  else
    log_warn "plugin install failed; you can retry manually:"
    log_warn "  claude plugin install claudebase@claudebase-dev"
  fi
}

# ============================================================================
# Pre-warm e5 encoder so first `claudebase ingest` doesn't pay ~30s cold start
# ============================================================================
preload_encoder() {
  local exe_ext=""
  case "$(uname -ms)" in MINGW*|MSYS*|CYGWIN*) exe_ext=".exe" ;; esac
  local bin="$CLAUDE_DIR/tools/claudebase/claudebase${exe_ext}"
  if [ ! -x "$bin" ]; then return 0; fi

  log_info "Pre-loading e5-multilingual-small encoder (~120 MB on first run)..."
  if "$bin" warmup --quiet 2>&1; then
    log_ok "encoder ready (cached at ~/.claude/tools/claudebase/models/)"
  else
    log_warn "encoder pre-load failed; fastembed will retry on first ingest"
  fi
}

# ============================================================================
# Main
# ============================================================================
echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}  claudebase v${CLAUDEBASE_VERSION} — installer${NC}"
echo -e "${BOLD}============================================${NC}"
echo ""
echo "  This will install to $CLAUDE_DIR:"
echo "    tools/claudebase/   (binary + pdfium + e5 model)"
echo "    rules/              (3 files — knowledge-base, knowledge-base-tool, tool-limitations)"
echo "    commands/           (3 files — knowledge-ingest, reflect, consolidate)"
echo "    agents/             (2 files — reflection, consolidator)"
echo ""

if ! confirm "Proceed with installation?"; then
  log_info "Aborted."
  exit 0
fi

get_source_dir
install_prompts
install_binary
register_alias
register_bash_allowlist
install_pdfium
preload_encoder
register_claude_plugin

# ============================================================================
# Optional post-install daemon hook (Slice 2 — STRUCTURAL-2-3)
# ============================================================================
# Opt-in via `CLAUDEBASE_INSTALL_DAEMON=1`. Fails soft: the post-install
# step never aborts the installer when it errors. `--no-start` keeps the
# install pre-reboot — the service comes up on the next login (systemd
# user unit + WantedBy=default.target).
if [ "${CLAUDEBASE_INSTALL_DAEMON:-0}" = "1" ]; then
  log_info "CLAUDEBASE_INSTALL_DAEMON=1 detected; installing daemon service unit..."
  if claudebase daemon install --no-start --yes; then
    log_ok "Daemon service unit installed (start at next login or via 'claudebase daemon start')"
  else
    log_warn "Daemon install failed; continuing without daemon (re-run later with 'claudebase daemon install')"
  fi
fi

# Cleanup the temp clone (only when we made one).
if [ "$LOCAL_MODE" = false ] && [ -n "$SCRIPT_DIR" ] && [ -d "$SCRIPT_DIR" ] && [ "$SCRIPT_DIR" != "/" ]; then
  rm -rf "$SCRIPT_DIR"
fi

echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}  claudebase install complete${NC}"
echo -e "${BOLD}============================================${NC}"
echo ""
echo "  Quick start:"
echo "    claudebase --version                  Confirm binary is on PATH"
echo "    claudebase ingest <path>              Ingest PDF/MD/TXT into <cwd>/.claude/knowledge/"
echo "    claudebase search '<query>' --json    Hybrid retrieval over the books corpus"
echo "    claudebase insight create '...' \\     Persist a cognitive insight (insights corpus)"
echo "        --type agent-learned --agent <name>"
echo "    claudebase compare '<query>'          A/B-test all 3 retrieval modes"
echo ""
echo "  Skills installed:"
echo "    /knowledge-ingest    Ingest a folder/file into the per-project knowledge base"
echo "    /reflect             DMN unfocused observation pass — user-invoked"
echo "    /consolidate         Cross-artifact drift detection (auto-chained between waves)"
echo ""
echo "  Agents installed:"
echo "    reflection (Drift)       Default Mode Network observation pass"
echo "    consolidator (Mnem)      Hippocampal-replay drift detection"
echo ""
echo "  Tip: re-ingest existing PDFs (\`claudebase ingest <path>\`) to upgrade"
echo "  pre-v2 indexes to schema v3 — that's what unlocks per-page citations."
echo ""
