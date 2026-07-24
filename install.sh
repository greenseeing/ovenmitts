#!/usr/bin/env bash
# ovenmitts installer — safe to re-run; upgrades in place when a newer release exists.
#
#   curl -fsSL https://raw.githubusercontent.com/greenseeing/ovenmitts/main/install.sh | bash
#
set -euo pipefail

REPO="greenseeing/ovenmitts"
BIN="ovenmitts"

# --- output helpers -------------------------------------------------------
if [ -t 1 ]; then
  C_STEP=$'\033[36m'; C_OK=$'\033[32m'; C_WARN=$'\033[33m'; C_ERR=$'\033[31m'; C_OFF=$'\033[0m'
else
  C_STEP=""; C_OK=""; C_WARN=""; C_ERR=""; C_OFF=""
fi
step() { printf '%s==>%s %s\n' "$C_STEP" "$C_OFF" "$*"; }
ok()   { printf '%s ok%s  %s\n' "$C_OK" "$C_OFF" "$*"; }
warn() { printf '%swarning:%s %s\n' "$C_WARN" "$C_OFF" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }

# --- detection ------------------------------------------------------------
detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo "amd64" ;;
    aarch64 | arm64) echo "arm64" ;;
    *) die "unsupported CPU architecture: $(uname -m)" ;;
  esac
}

# Echo the system package manager, or nothing if none is recognised.
detect_pm() {
  for pm in apt-get dnf yum zypper pacman; do
    if command -v "$pm" >/dev/null 2>&1; then
      echo "$pm"
      return 0
    fi
  done
  return 0
}

# Run a privileged command via sudo when not already root.
as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    die "this step needs root; install sudo or re-run as root"
  fi
}

# Install one package, trying each candidate name until one succeeds.
pm_install() {
  local pm="$1"; shift
  local pkg
  for pkg in "$@"; do
    case "$pm" in
      apt-get) as_root apt-get install -y "$pkg" >/dev/null 2>&1 && return 0 ;;
      dnf | yum) as_root "$pm" install -y "$pkg" >/dev/null 2>&1 && return 0 ;;
      zypper) as_root zypper --non-interactive install "$pkg" >/dev/null 2>&1 && return 0 ;;
      pacman) as_root pacman -S --noconfirm "$pkg" >/dev/null 2>&1 && return 0 ;;
    esac
  done
  return 1
}

# One backend: install unless the command is already present, warn on failure.
install_backend() {
  local pm="$1" cmd="$2" label="$3"; shift 3
  step "Installing $label"
  if command -v "$cmd" >/dev/null 2>&1; then
    ok "$cmd already installed"
  elif pm_install "$pm" "$@"; then
    ok "$cmd installed"
  else
    warn "could not install '$cmd' automatically — $label stays unavailable until you install it"
  fi
}

install_backends() {
  local pm="$1"
  if [ -z "$pm" ]; then
    warn "no known package manager found; install xorriso (required) and par2, udisks2, eject yourself"
    return 0
  fi
  if [ "$pm" = "apt-get" ]; then
    step "Refreshing package lists"
    as_root apt-get update -y >/dev/null 2>&1 || true
  fi

  install_backend "$pm" xorriso "the burn engine (xorriso, required)" xorriso
  install_backend "$pm" par2 "parity (par2)" par2 par2cmdline
  install_backend "$pm" udisksctl "rootless verify mounts (udisks2)" udisks2
  install_backend "$pm" eject "tray control (eject)" eject util-linux
}

# --- versions -------------------------------------------------------------
latest_version() {
  # An explicit pin skips the API entirely.
  if [ -n "${OVENMITTS_VERSION:-}" ]; then
    printf '%s' "${OVENMITTS_VERSION#v}"
    return 0
  fi
  # Resolve the newest release tag (e.g. v0.1.0 -> 0.1.0) via the GitHub API.
  local body
  body="$(curl -sSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null || true)"
  printf '%s' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1
}

installed_version() {
  if command -v "$BIN" >/dev/null 2>&1; then
    "$BIN" --version 2>/dev/null | awk '{print $2}'
  fi
}

# --- install --------------------------------------------------------------
# Reuse an existing install's directory so re-runs replace in place instead of
# shadowing it; otherwise default to ~/.local/bin (no sudo for the common case).
choose_bindir() {
  local existing
  existing="$(command -v "$BIN" 2>/dev/null || true)"
  if [ -n "$existing" ]; then
    dirname "$(readlink -f "$existing")"
  else
    echo "$HOME/.local/bin"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

verify_checksum() {
  local file="$1" url="$2"
  local sums expected actual
  sums="$(curl -fsSL "${url}.sha256" 2>/dev/null || true)"
  if [ -z "$sums" ]; then
    die "no published checksum for this release — refusing to install unverified"
  fi
  expected="$(printf '%s' "$sums" | awk '{print $1}')"
  actual="$(sha256_of "$file")"
  if [ -z "$actual" ]; then
    die "no sha256 tool found (install coreutils) — refusing to install unverified"
  fi
  [ "$expected" = "$actual" ] || die "checksum mismatch — refusing to install (expected $expected, got $actual)"
  ok "checksum verified"
}

# Download + atomically replace the binary. The temp file is staged on the SAME
# filesystem as the target so the final move is an atomic rename — this
# sidesteps ETXTBSY ("Text file busy") when replacing a running binary.
install_binary() {
  local version="$1" arch="$2" bindir="$3"
  local url="https://github.com/$REPO/releases/download/v${version}/${BIN}-linux-${arch}"
  step "Downloading $BIN v$version ($arch)"

  mkdir -p "$bindir" 2>/dev/null || as_root mkdir -p "$bindir"

  local tmp writable=0
  [ -w "$bindir" ] && writable=1
  if [ "$writable" -eq 1 ]; then
    tmp="$bindir/.$BIN.new.$$"
  else
    tmp="$(mktemp)"
  fi

  if ! curl -fSL --progress-bar -o "$tmp" "$url"; then
    rm -f "$tmp" 2>/dev/null || true
    die "download failed: $url"
  fi
  verify_checksum "$tmp" "$url"
  chmod +x "$tmp"

  if [ "$writable" -eq 1 ]; then
    mv -f "$tmp" "$bindir/$BIN"
  else
    # Stage inside $bindir (same filesystem) so the final replace is an atomic
    # rename. A cross-filesystem mv from /tmp is not atomic and can hit ETXTBSY
    # when replacing a running binary.
    local stage="$bindir/.$BIN.new.$$"
    as_root cp "$tmp" "$stage"
    as_root chmod +x "$stage"
    as_root mv -f "$stage" "$bindir/$BIN"
    rm -f "$tmp" 2>/dev/null || true
  fi
  ok "installed to $bindir/$BIN"

  case ":$PATH:" in
    *":$bindir:"*) ;;
    *) warn "$bindir is not on your PATH — add it with:  export PATH=\"$bindir:\$PATH\"" ;;
  esac
}

backends_present() {
  command -v xorriso >/dev/null 2>&1 && command -v par2 >/dev/null 2>&1
}

main() {
  command -v curl >/dev/null 2>&1 || die "curl is required"

  local arch pm latest current bindir
  arch="$(detect_arch)"
  latest="$(latest_version)"
  current="$(installed_version)"

  if [ -z "$latest" ]; then
    die "could not determine the latest release (is the network up?)"
  fi

  # Nothing to do: skip the package manager entirely so a re-run is a fast no-op.
  if [ "$current" = "$latest" ] && backends_present; then
    ok "$BIN is already up to date (v$current) and ready to use"
    return 0
  fi

  pm="$(detect_pm)"
  install_backends "$pm"

  bindir="$(choose_bindir)"
  if [ "$current" = "$latest" ]; then
    ok "$BIN v$current is already installed"
  else
    [ -n "$current" ] && step "Upgrading $BIN: v$current -> v$latest"
    install_binary "$latest" "$arch" "$bindir"
  fi

  if ! id -nG 2>/dev/null | tr ' ' '\n' | grep -qx cdrom; then
    warn "you are not in the 'cdrom' group — burning as non-root needs:  sudo usermod -aG cdrom \$USER  (then log out and back in)"
  fi

  printf '\n'
  ok "Done. Run '$BIN' to start."
}

# Run the installer only when executed (directly or via `curl ... | bash`), not
# when sourced — sourcing exposes single functions without performing an install.
if ! (return 0 2>/dev/null); then
  main "$@"
fi
