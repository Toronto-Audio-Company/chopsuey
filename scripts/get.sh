#!/usr/bin/env bash
# One-line install: clones chopsuey to a temporary folder, runs
# scripts/install.sh there, then deletes the clone.
#
#   curl -fsSL https://raw.githubusercontent.com/Toronto-Audio-Company/chopsuey/main/scripts/get.sh | bash
#
# CHOPSUEY_REPO and CHOPSUEY_REF pick another repository (URL or local path)
# or branch, e.g. to try an unmerged branch.
#
# Everything is inside main(), called on the last line, so a download cut
# off halfway runs nothing.
set -euo pipefail

# Makes sure cargo is on PATH, installing Rust with rustup (the official
# installer, https://rustup.rs) if it isn't there. rustup puts it in
# ~/.cargo and adds that to the shell profile, so later terminals have it too.
install_rust() {
  local env="${CARGO_HOME:-$HOME/.cargo}/env"
  # Installed but not on this shell's PATH (e.g. a profile that wasn't read).
  if ! command -v cargo >/dev/null && [[ -f "$env" ]]; then
    # shellcheck source=/dev/null
    source "$env"
  fi
  command -v cargo >/dev/null && return

  printf '\n== Installing Rust (needed to build chopsuey)\n'
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck source=/dev/null
  source "$env"
  command -v cargo >/dev/null || {
    echo "error: Rust installed, but cargo still isn't on PATH. Open a new terminal and run this again." >&2
    exit 1
  }
}

main() {
  local repo="${CHOPSUEY_REPO:-https://github.com/Toronto-Audio-Company/chopsuey.git}"
  local ref="${CHOPSUEY_REF:-main}"

  # Checked before cloning, so a missing tool fails in a second rather than
  # after the download. The Command Line Tools bring git, and the linker,
  # swift and codesign the build needs. /usr/bin/git exists without them (it
  # only offers to install them), so ask xcode-select instead.
  xcode-select -p >/dev/null 2>&1 || {
    xcode-select --install 2>/dev/null || true
    echo "error: chopsuey needs Apple's Command Line Tools. Finish the install macOS just opened, then run this again." >&2
    exit 1
  }

  install_rust

  # Global, not local: the trap runs after main() has returned.
  tmp="$(mktemp -d -t chopsuey)"
  # Removed however the script ends: done, failed or Ctrl-C.
  trap 'rm -rf "$tmp"' EXIT

  printf '\n== Downloading chopsuey (%s)\n' "$ref"
  git clone --quiet --depth 1 --branch "$ref" "$repo" "$tmp/chopsuey"

  # install.sh copies the finished plugin into ~/Library, so nothing it
  # installs lives in the clone.
  "$tmp/chopsuey/scripts/install.sh"

  printf '\n== Cleaning up\n'
}

main "$@"
