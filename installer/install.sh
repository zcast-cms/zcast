#!/bin/sh
# zcast one-line installer (POSIX sh)
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/zcast-cms/zcast/main/installer/install.sh | sh
#
# Optional environment overrides:
#   ZCAST_VERSION=v0.4.0       Pin a specific release tag (default: latest)
#   ZCAST_INSTALL_DIR=...      Target directory for the binary
#   ZCAST_SKIP_SETUP=1         Install binary only without launching `zcast setup`
#   ZCAST_REPO=zcast-cms/zcast Override GitHub repository

set -eu

REPO="${ZCAST_REPO:-zcast-cms/zcast}"
VERSION="${ZCAST_VERSION:-latest}"

log() {
  printf '%s\n' "$*"
}

err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "required command '$1' not found"
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64)   printf 'x86_64-unknown-linux-musl' ;;
        aarch64|arm64)  printf 'aarch64-unknown-linux-musl' ;;
        *) err "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64|aarch64)  printf 'aarch64-apple-darwin' ;;
        x86_64)         printf 'x86_64-apple-darwin' ;;
        *) err "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    *)
      err "unsupported operating system: $os (Linux and macOS are supported)"
      ;;
  esac
}

verify_sha256() {
  file="$1"
  expected="$2"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else
    err "neither 'sha256sum' nor 'shasum' is available to verify download integrity"
  fi
  [ "$actual" = "$expected" ] || err "SHA-256 mismatch for $file (expected $expected, got $actual)"
}

choose_install_dir() {
  if [ -n "${ZCAST_INSTALL_DIR:-}" ]; then
    printf '%s' "$ZCAST_INSTALL_DIR"
    return
  fi
  if [ -w "/usr/local/bin" ]; then
    printf '/usr/local/bin'
  elif [ "$(id -u)" -eq 0 ]; then
    printf '/usr/local/bin'
  elif command -v sudo >/dev/null 2>&1 && [ -e /dev/tty ]; then
    printf '/usr/local/bin'
  else
    printf '%s/.local/bin' "$HOME"
  fi
}

main() {
  need_cmd curl
  need_cmd tar
  need_cmd awk

  # Idempotent: if zcast is already installed and configured, run `zcast update` instead.
  if [ -z "${ZCAST_FORCE_INSTALL:-}" ] && command -v zcast >/dev/null 2>&1; then
    if [ -f "/etc/zcast/zcast.toml" ] || [ -f "./zcast.toml" ]; then
      log "Existing zcast installation detected — running 'zcast update'..."
      exec zcast update
    fi
  fi

  target="$(detect_target)"
  archive="zcast-${target}.tar.gz"

  if [ "$VERSION" = "latest" ]; then
    base_url="https://github.com/${REPO}/releases/latest/download"
  else
    base_url="https://github.com/${REPO}/releases/download/${VERSION}"
  fi

  tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t zcast-install)"
  trap 'rm -rf "$tmpdir"' EXIT INT TERM

  log "Downloading zcast (${VERSION}) for ${target}..."
  curl -fsSL "${base_url}/${archive}" -o "${tmpdir}/${archive}"
  curl -fsSL "${base_url}/SHA256SUMS" -o "${tmpdir}/SHA256SUMS"

  expected_sum="$(awk -v name="$archive" '$2 == name || $2 == ("*" name) { print $1; exit }' "${tmpdir}/SHA256SUMS")"
  [ -n "$expected_sum" ] || err "could not find checksum entry for ${archive} in SHA256SUMS"

  verify_sha256 "${tmpdir}/${archive}" "$expected_sum"
  log "Verified SHA-256 checksum."

  tar -xzf "${tmpdir}/${archive}" -C "$tmpdir"
  [ -f "${tmpdir}/zcast" ] || err "archive did not contain expected 'zcast' binary"
  chmod 755 "${tmpdir}/zcast"

  install_dir="$(choose_install_dir)"
  dest="${install_dir}/zcast"

  if [ -d "$install_dir" ] && [ -w "$install_dir" ]; then
    mv -f "${tmpdir}/zcast" "$dest"
  elif [ "$install_dir" = "/usr/local/bin" ] && command -v sudo >/dev/null 2>&1; then
    log "Installing zcast to ${dest} (requires sudo)..."
    sudo mkdir -p "$install_dir"
    sudo mv -f "${tmpdir}/zcast" "$dest"
  else
    mkdir -p "$install_dir"
    mv -f "${tmpdir}/zcast" "$dest"
  fi

  log "Installed zcast to ${dest}"

  case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
      log "Note: add ${install_dir} to your PATH (e.g. export PATH=\"${install_dir}:\$PATH\")."
      ;;
  esac

  if [ "${ZCAST_SKIP_SETUP:-0}" = "1" ]; then
    log "Run '${dest} setup' when ready to configure zcast."
    exit 0
  fi

  # Reattach stdin to /dev/tty so interactive `zcast setup` works under `curl | sh`.
  if [ -e /dev/tty ]; then
    exec "$dest" setup < /dev/tty
  else
    log "zcast installed. Run '${dest} setup' to configure."
  fi
}

main "$@"
