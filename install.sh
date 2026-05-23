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
CLAUDEBASE_VERSION="0.6.0"
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
# Install whisper-cli + ffmpeg (voice transcription dependencies)
# ============================================================================
# Needed by the upcoming Rust port of the official Telegram plugin which
# transcribes inbound voice messages locally via whisper.cpp before
# emitting them as channel notifications.
#
# Best-effort:
#   - If both binaries are already on PATH → log + return 0 (idempotent).
#   - If a package manager is detected → attempt install; warn on failure.
#   - If no package manager → log clear manual-install hint + return 0.
# The actual whisper model (~1.5 GB ggml-medium.bin) is NOT downloaded
# here — too heavy for the install path. The plugin downloads it lazily
# on first voice message, or the operator drops it at
# ~/.local/share/whisper-cpp/models/ggml-medium.bin ahead of time.
#
# Opt-out: set CLAUDEBASE_SKIP_WHISPER=1 to bypass entirely (no install,
# no log spam). For headless CI where audio deps would just add minutes
# to the install for no benefit.
install_whisper_stack() {
  if [ "${CLAUDEBASE_SKIP_WHISPER:-0}" = "1" ]; then
    log_info "CLAUDEBASE_SKIP_WHISPER=1 — skipping ffmpeg + whisper-cli install"
    return 0
  fi

  local need_ffmpeg=true
  local need_whisper=true
  command -v ffmpeg >/dev/null 2>&1 && need_ffmpeg=false
  command -v whisper-cli >/dev/null 2>&1 && need_whisper=false

  if ! $need_ffmpeg && ! $need_whisper; then
    log_ok "ffmpeg + whisper-cli already on PATH (voice transcription ready)"
    return 0
  fi

  # Detect package manager (try most reliable first).
  local pkg_mgr=""
  local pkg_install=""
  local pkg_ffmpeg="ffmpeg"
  local pkg_whisper="whisper-cpp"
  if command -v brew >/dev/null 2>&1; then
    pkg_mgr="brew"
    pkg_install="brew install"
  elif command -v apt-get >/dev/null 2>&1; then
    pkg_mgr="apt-get"
    pkg_install="sudo apt-get install -y"
  elif command -v dnf >/dev/null 2>&1; then
    pkg_mgr="dnf"
    pkg_install="sudo dnf install -y"
  elif command -v pacman >/dev/null 2>&1; then
    pkg_mgr="pacman"
    pkg_install="sudo pacman -S --noconfirm"
  else
    log_warn "no supported package manager detected (brew/apt-get/dnf/pacman); voice transcription disabled"
    log_warn "  to enable, install manually:"
    log_warn "    macOS:  brew install whisper-cpp ffmpeg"
    log_warn "    Linux:  apt install whisper-cpp ffmpeg  (or dnf/pacman equivalent)"
    return 0
  fi

  if $need_ffmpeg; then
    log_info "installing ffmpeg via $pkg_mgr..."
    if $pkg_install $pkg_ffmpeg >/dev/null 2>&1; then
      log_ok "ffmpeg installed"
    else
      log_warn "ffmpeg install via $pkg_mgr failed; install manually: $pkg_install $pkg_ffmpeg"
    fi
  fi

  if $need_whisper; then
    log_info "installing whisper-cli via $pkg_mgr (this can take a few minutes)..."
    if $pkg_install $pkg_whisper >/dev/null 2>&1; then
      log_ok "whisper-cli installed"
    else
      log_warn "whisper-cli install via $pkg_mgr failed; install manually: $pkg_install $pkg_whisper"
    fi
  fi

  if command -v ffmpeg >/dev/null 2>&1 && command -v whisper-cli >/dev/null 2>&1; then
    log_info "voice transcription stack ready — whisper model auto-downloads on first voice msg"
    log_info "  (or pre-download to ~/.local/share/whisper-cpp/models/ggml-medium.bin)"
  fi
  return 0
}

# ============================================================================
# Install the Rust port of the official Anthropic Telegram plugin.
# ============================================================================
# Dev strategy (per operator brief — 2026-05-23):
#   1. install the OFFICIAL upstream plugin (telegram@claude-plugins-official)
#   2. cargo-build our Rust binary from plugins/telegram-rs/ in this repo
#   3. copy it into the plugin cache as `server-rs` alongside upstream `server.ts`
#   4. patch `.mcp.json` with a bash toggle that defaults to Rust (server-rs)
#      and falls back to bun (TSX) if env var TELEGRAM_USE_TSX_SERVER=1 OR
#      if the Rust binary is missing
#
# Skipped (best-effort):
#   - `claude` CLI not on PATH → log + return 0 (no plugin to patch into)
#   - `cargo` not on PATH → log + return 0 (operator can install Rust later)
#   - CLAUDEBASE_SKIP_TELEGRAM=1 → silent skip (for headless CI)
#
# Idempotent: re-running just rebuilds binary (cargo cache), recopies, re-patches.
# Backup of upstream `.mcp.json` is preserved at `.mcp.json.upstream-backup`.
# ============================================================================
install_telegram_plugin() {
  if [ "${CLAUDEBASE_SKIP_TELEGRAM:-0}" = "1" ]; then
    log_info "CLAUDEBASE_SKIP_TELEGRAM=1 — skipping telegram plugin install"
    return 0
  fi

  if ! command -v claude >/dev/null 2>&1; then
    log_info "claude CLI not on PATH; skipping telegram plugin install"
    log_info "  to install manually after Claude Code is installed:"
    log_info "    claude plugin install telegram@claude-plugins-official"
    log_info "    then re-run this installer to patch the Rust binary"
    return 0
  fi

  # ----- 1. Install official plugin if not already -----
  local marketplace_already=false
  if claude plugin marketplace list 2>/dev/null | grep -q "claude-plugins-official"; then
    marketplace_already=true
  fi
  if [ "$marketplace_already" = false ]; then
    log_info "Adding marketplace anthropics/claude-plugins-official..."
    claude plugin marketplace add anthropics/claude-plugins-official 2>&1 | tail -2 || true
  fi
  log_info "Installing telegram@claude-plugins-official (idempotent)..."
  claude plugin install telegram@claude-plugins-official 2>&1 | tail -2 || true

  # ----- 2. Locate the installed plugin dir -----
  # Prefer installed_plugins.json (authoritative — points to the currently
  # ACTIVE version, not whatever orphan dirs leftover in cache). Fall back
  # to newest-version glob if jq/python3 absent or manifest unreadable.
  local plugin_root="$CLAUDE_DIR/plugins/cache/claude-plugins-official/telegram"
  if [ ! -d "$plugin_root" ]; then
    log_warn "official telegram plugin not found at $plugin_root after install — skipping Rust patch"
    return 0
  fi
  local plugin_dir=""
  local installed_manifest="$CLAUDE_DIR/plugins/installed_plugins.json"
  if [ -f "$installed_manifest" ]; then
    if command -v jq >/dev/null 2>&1; then
      plugin_dir=$(jq -r '.plugins["telegram@claude-plugins-official"][0].installPath // empty' "$installed_manifest" 2>/dev/null)
    elif command -v python3 >/dev/null 2>&1; then
      plugin_dir=$(python3 -c "import json,sys; d=json.load(open('$installed_manifest')); p=d.get('plugins',{}).get('telegram@claude-plugins-official',[]); print(p[0]['installPath'] if p else '')" 2>/dev/null)
    fi
  fi
  if [ -z "$plugin_dir" ] || [ ! -d "$plugin_dir" ]; then
    # Fallback: newest version subdir (semver-sortable).
    local version_dir
    version_dir=$(ls -1 "$plugin_root" 2>/dev/null | sort -V | tail -1)
    if [ -z "$version_dir" ] || [ ! -d "$plugin_root/$version_dir" ]; then
      log_warn "no version subdir found under $plugin_root — skipping Rust patch"
      return 0
    fi
    plugin_dir="$plugin_root/$version_dir"
    log_info "manifest lookup unavailable; falling back to newest-version glob: $plugin_dir"
  fi
  log_info "patching plugin at $plugin_dir"

  # ----- 3. Resolve binary: download from GH release first; fall back to
  #         cargo build only if download fails (e.g. offline, asset missing
  #         for this platform, claudebase version with no telegram-plugin-rs
  #         artifacts yet). Cargo fallback requires `cargo` on PATH AND the
  #         repo's plugins/telegram-rs/ source tree (present in local-mode
  #         install or fresh clone). -----
  local platform=""
  local exe_ext=""
  case "$(uname -ms)" in
    "Darwin arm64")  platform="darwin-arm64"  ;;
    "Darwin x86_64") platform="darwin-x64"    ;;
    "Linux x86_64")  platform="linux-x64"     ;;
    "Linux aarch64") platform="linux-arm64"   ;;
    MINGW*|MSYS*|CYGWIN*)
      case "$(uname -m)" in
        x86_64) platform="windows-x64"; exe_ext=".exe" ;;
        *)      log_warn "unsupported Windows arch: $(uname -m); skipping telegram-plugin-rs"; return 0 ;;
      esac
      ;;
    *) log_warn "telegram-plugin-rs binary unavailable for $(uname -ms); skipping"; return 0 ;;
  esac

  local target_bin="$plugin_dir/server-rs${exe_ext}"
  local url="${RELEASE_BASE}/claudebase-v${CLAUDEBASE_VERSION}/telegram-plugin-rs-${platform}${exe_ext}"
  local downloaded=false
  local tmp_download
  tmp_download="$(mktemp)"

  log_info "downloading telegram-plugin-rs binary from GH release for $platform..."
  if command -v curl >/dev/null 2>&1; then
    if curl --proto '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 120 "$url" -o "$tmp_download" 2>/dev/null; then
      downloaded=true
    fi
  elif command -v wget >/dev/null 2>&1; then
    if wget --https-only --secure-protocol=TLSv1_2 --max-redirect=5 --timeout=120 -q -O "$tmp_download" "$url" 2>/dev/null; then
      downloaded=true
    fi
  fi

  if [ "$downloaded" = true ] && [ -s "$tmp_download" ]; then
    mv "$tmp_download" "$target_bin"
    chmod 0755 "$target_bin"
    log_ok "server-rs downloaded ($(wc -c <"$target_bin" | tr -d ' ') bytes) → $target_bin"
  else
    rm -f "$tmp_download"
    log_warn "telegram-plugin-rs download failed — falling back to cargo build"
    if ! command -v cargo >/dev/null 2>&1; then
      log_warn "  cargo not on PATH either; install Rust (https://rustup.rs/) or wait for a release with telegram-plugin-rs artifacts"
      return 0
    fi
    if [ ! -d "$SCRIPT_DIR/plugins/telegram-rs" ]; then
      log_warn "  plugins/telegram-rs source not present at $SCRIPT_DIR — skipping"
      return 0
    fi
    log_info "cargo build --release -p telegram-plugin-rs (first build ~5 min, cached after)"
    if ! ( cd "$SCRIPT_DIR" && cargo build --release -p telegram-plugin-rs 2>&1 | tail -3 ); then
      log_warn "cargo build telegram-plugin-rs failed; the upstream TSX plugin will run unchanged"
      return 0
    fi
    local built_bin="$SCRIPT_DIR/target/release/telegram-plugin-rs${exe_ext}"
    if [ ! -x "$built_bin" ]; then
      log_warn "build succeeded but binary missing at $built_bin — skipping patch"
      return 0
    fi
    cp "$built_bin" "$target_bin"
    chmod 0755 "$target_bin"
    log_ok "server-rs built locally ($(wc -c <"$built_bin" | tr -d ' ') bytes) → $target_bin"
  fi

  # ----- 5. Patch .mcp.json with toggle (backup upstream version first) -----
  local mcp_json="$plugin_dir/.mcp.json"
  local mcp_backup="$plugin_dir/.mcp.json.upstream-backup"
  if [ -f "$mcp_json" ] && [ ! -f "$mcp_backup" ]; then
    cp "$mcp_json" "$mcp_backup"
    log_ok ".mcp.json.upstream-backup preserved"
  fi
  cat > "$mcp_json" <<'EOF'
{
  "mcpServers": {
    "telegram": {
      "command": "bash",
      "args": [
        "-c",
        "if [ -z \"$TELEGRAM_USE_TSX_SERVER\" ] && [ -x \"$CLAUDE_PLUGIN_ROOT/server-rs\" ]; then exec \"$CLAUDE_PLUGIN_ROOT/server-rs\" 2>>/tmp/telegram-rs.log; else exec bun run --cwd \"$CLAUDE_PLUGIN_ROOT\" --shell=bun --silent start; fi"
      ]
    }
  }
}
EOF
  chmod 0644 "$mcp_json"
  log_ok ".mcp.json patched (Rust by default; TELEGRAM_USE_TSX_SERVER=1 falls back to bun)"

  log_info "to enable: launch Claude Code with"
  log_info "  claude --channels plugin:telegram@claude-plugins-official"
  log_info "Rust binary stderr → /tmp/telegram-rs.log"
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
install_whisper_stack
preload_encoder
register_claude_plugin
install_telegram_plugin

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
