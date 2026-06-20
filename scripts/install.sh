#!/bin/sh
# tower installer — downloads a prebuilt binary from GitHub Releases.
#
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://raw.githubusercontent.com/awf-project/tower/main/scripts/install.sh | sh
#
# Environment overrides:
#   TOWER_VERSION      Which build to install (default: latest stable). One of:
#                        latest | stable  -> newest production release (v* tag)
#                        dev              -> newest development prerelease (dev-<sha>)
#                        vX.Y.Z | dev-... -> a specific tag, used verbatim
#   TOWER_INSTALL_DIR  Target directory for the binary (default: $HOME/.local/bin)
#   TOWER_TARGET       Force a target triple instead of auto-detecting
#   TOWER_EXTENSIONS_DIR  Where to install the bundled reference extensions
#                         (default: the global XDG scope the host discovers,
#                          e.g. ~/.local/share/tower/extensions). Matches the
#                          host's own scope-override env var.
#
# Examples:
#   curl -fsSL .../install.sh | sh                      # latest stable
#   curl -fsSL .../install.sh | TOWER_VERSION=dev sh    # rolling dev build
#   curl -fsSL .../install.sh | TOWER_VERSION=v0.1.0 sh # pinned version
#
# Decision: download a prebuilt binary rather than building from source.
# Why: zero toolchain on the user's machine, install in seconds.
# Trade-off: depends on a release pipeline publishing assets named
#   tower-<tag>-<target>.tar.gz (+ .sha256). `make dist` produces exactly those.
set -eu

REPO="awf-project/tower"
BIN="tower"
INSTALL_DIR="${TOWER_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'install: %s\n' "$1" >&2; exit 1; }
info() { printf 'install: %s\n' "$1" >&2; }

# --- download helper: curl or wget, whichever exists ------------------------
if command -v curl >/dev/null 2>&1; then
  dl() { curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
  dl_stdout() { curl --proto '=https' --tlsv1.2 -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  dl_stdout() { wget -qO - "$1"; }
else
  err "need curl or wget"
fi

# --- detect host target triple ----------------------------------------------
detect_target() {
  [ -n "${TOWER_TARGET:-}" ] && { printf '%s' "$TOWER_TARGET"; return; }

  os="$(uname -s)"
  arch="$(uname -m)"

  case "$arch" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) err "unsupported architecture: $arch (set TOWER_TARGET to override)" ;;
  esac

  case "$os" in
    Linux) printf '%s-unknown-linux-gnu' "$arch" ;;
    Darwin) printf '%s-apple-darwin' "$arch" ;;
    *) err "unsupported OS: $os (set TOWER_TARGET to override)" ;;
  esac
}

# --- resolve release tag -----------------------------------------------------
# `latest`/`stable` -> newest production release via the API.
# `dev`             -> newest `dev-*` prerelease (releases are returned newest-first).
# anything else     -> used verbatim (the asset name embeds the tag).
resolve_version() {
  ver="${TOWER_VERSION:-latest}"
  case "$ver" in
    latest | stable)
      tag="$(dl_stdout "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | cut -d'"' -f4)"
      [ -n "$tag" ] || err "could not resolve latest release; set TOWER_VERSION (e.g. v0.1.0 or dev)"
      printf '%s' "$tag"
      ;;
    dev)
      tag="$(dl_stdout "https://api.github.com/repos/$REPO/releases?per_page=30" \
        | grep '"tag_name"' | cut -d'"' -f4 | grep '^dev-' | head -1)"
      [ -n "$tag" ] || err "no dev release found; set TOWER_VERSION to a specific tag"
      printf '%s' "$tag"
      ;;
    *)
      printf '%s' "$ver"
      ;;
  esac
}

# --- main --------------------------------------------------------------------
TARGET="$(detect_target)"
VERSION="$(resolve_version)"

asset="${BIN}-${VERSION}-${TARGET}.tar.gz"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "installing $BIN $VERSION ($TARGET)"
dl "$base/$asset" "$tmp/$asset" || err "download failed: $base/$asset"

# Verify checksum when the release publishes one (non-fatal if absent).
if dl "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
  expected="$(cut -d' ' -f1 < "$tmp/$asset.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
  else
    actual=""
    info "no sha256 tool found; skipping checksum verification"
  fi
  [ -z "$actual" ] || [ "$actual" = "$expected" ] || err "checksum mismatch for $asset"
else
  info "no checksum published; skipping verification"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/$BIN" ] || err "archive did not contain a '$BIN' binary"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/$BIN" "$INSTALL_DIR/$BIN"
info "installed to $INSTALL_DIR/$BIN"

# --- install bundled reference extensions, if the archive carries them -------
# Tarball layout: extensions/<name>/{extension.toml,<name>_extension}. They go
# into the global discovery scope so a fresh install has AST/LSP/fmt without a
# source checkout. The host discovers this path as the global XDG scope
# (directories::ProjectDirs "tower" data_dir + /extensions); $TOWER_EXTENSIONS_DIR
# overrides it, matching the host's own scope-override env var.
if [ -d "$tmp/extensions" ]; then
  if [ -n "${TOWER_EXTENSIONS_DIR:-}" ]; then
    ext_dir="$TOWER_EXTENSIONS_DIR"
  else
    case "$(uname -s)" in
      Darwin) ext_dir="$HOME/Library/Application Support/tower/extensions" ;;
      *)      ext_dir="${XDG_DATA_HOME:-$HOME/.local/share}/tower/extensions" ;;
    esac
  fi
  for d in "$tmp"/extensions/*/; do
    [ -d "$d" ] || continue
    name="$(basename "$d")"
    # Replace the whole extension dir so a stale binary never lingers beside a
    # new manifest after an upgrade.
    rm -rf "$ext_dir/$name"
    mkdir -p "$ext_dir/$name"
    for f in "$d"*; do
      case "$(basename "$f")" in
        *.toml) install -m 0644 "$f" "$ext_dir/$name/" ;;
        *)      install -m 0755 "$f" "$ext_dir/$name/" ;;
      esac
    done
  done
  info "installed reference extensions to $ext_dir"
fi

# PATH hint.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) info "note: $INSTALL_DIR is not on your PATH — add it to use '$BIN' directly" ;;
esac

"$INSTALL_DIR/$BIN" --version 2>/dev/null || info "run '$BIN --workspace-dir <path>' to start the MCP server"
