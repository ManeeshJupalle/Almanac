#!/usr/bin/env bash
#
# Cloud-agent environment setup for Almanac.
#
# Goal: `cargo test -p almanac-core --locked` and `npm run build` both work on a
# fresh agent VM without any manual fixing. Everything here is idempotent, so a
# re-run on a warm snapshot is cheap.
#
# Not installed here: the ~580 MB of on-device models (MiniLM + Qwen). The
# model-dependent tests self-skip when `models/` is absent, so the suite is
# green without them; fetch them per the README only if you need to run the
# synthesis path for real.

set -euo pipefail

readonly MSRV_MINOR=91 # tract-onnx 0.23 requires rustc 1.91

log() { printf '\n=== %s\n' "$*"; }

# ---------------------------------------------------------------- apt packages

# The base image ships stale package lists, which makes real packages (e.g.
# libstdc++-14-dev, in noble-updates/universe) look like they don't exist.
# Refresh first or the C++ fix below silently can't be applied.
log "Refreshing package lists"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq

# Tauri's Linux system deps — kept in step with the workspace-build job in
# .github/workflows/ci.yml. Needed to compile src-tauri (the shell lib), not
# almanac-core alone.
log "Installing build and Tauri Linux system dependencies"
sudo apt-get install -y -qq --no-install-recommends \
  build-essential \
  pkg-config \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  librsvg2-dev \
  libayatana-appindicator3-dev

# `cc` and `c++` on this image point at clang, and clang links against a GCC
# installation directory it picks itself — which is not necessarily the one
# whose -dev package is installed. When they disagree, esaxx-rs (pulled in by
# tokenizers, via tract) fails to compile on a missing <cstdint> and then to
# link on a missing -lstdc++. Install the -dev package clang actually selected
# rather than overriding CC/CXX, so plain `cargo test` works unmodified.
cxx_probe() {
  printf '#include <cstdint>\nint main() { return 0; }\n' \
    | c++ -x c++ - -o /tmp/almanac-cxx-probe 2>/dev/null
}

log "Checking the C++ toolchain (esaxx-rs needs libstdc++ headers + .so)"
if cxx_probe; then
  echo "c++ already compiles and links against libstdc++"
else
  # `|| true`: reach the explicit error below rather than aborting on set -e if
  # the driver is missing or reports nothing (i.e. it is gcc, not clang).
  selected=$(c++ -v -E -x c++ /dev/null 2>&1 |
    sed -n 's|.*Selected GCC installation: .*/\([0-9][0-9]*\)$|\1|p' | tail -1 || true)
  # Fallback covers a `c++` that reports no selection (i.e. it is gcc, not
  # clang) and any future image whose GCC major we don't recognise.
  for ver in ${selected:-} 14 13; do
    echo "trying libstdc++-${ver}-dev"
    if sudo apt-get install -y -qq "libstdc++-${ver}-dev" 2>/dev/null && cxx_probe; then
      echo "installed libstdc++-${ver}-dev"
      break
    fi
  done
  # Fail loudly: a silent miss here resurfaces ~10 minutes into a cold build.
  cxx_probe || {
    echo "ERROR: c++ still cannot compile and link a <cstdint> program." >&2
    echo "       esaxx-rs will fail; see 'Selected GCC installation' in: c++ -v -E -x c++ /dev/null" >&2
    exit 1
  }
fi
rm -f /tmp/almanac-cxx-probe

# --------------------------------------------------------------------- rust

# The image's default toolchain is older than the workspace can build: tract
# requires 1.91, and Cargo.lock pins dependencies (dlopen2) that need the
# edition2024 feature. CI runs dtolnay/rust-toolchain@stable, so stable is the
# toolchain to match.
log "Installing and defaulting to Rust stable"
rustup toolchain install stable --profile minimal --no-self-update
rustup default stable

minor=$(rustc --version | sed -n 's|^rustc 1\.\([0-9][0-9]*\)\..*|\1|p')
if [ -z "$minor" ] || [ "$minor" -lt "$MSRV_MINOR" ]; then
  echo "ERROR: need rustc >= 1.${MSRV_MINOR} (tract-onnx), got: $(rustc --version)" >&2
  exit 1
fi
echo "$(rustc --version) — ok"

# Populate the registry cache so a later build doesn't also pay for downloads.
log "Fetching Rust dependencies"
cargo fetch --locked

# ---------------------------------------------------------------------- node

log "Installing npm dependencies"
npm ci

# ------------------------------------------------------------------- prebuild

# Off by default: a warm target/ is ~8 GB, which is a lot of snapshot for the
# time it saves. Set ALMANAC_PREBUILD=1 in the environment if agents on this
# repo spend most of their time waiting on cold Rust builds.
if [ "${ALMANAC_PREBUILD:-0}" = "1" ]; then
  log "Prebuilding almanac-core test binaries (ALMANAC_PREBUILD=1)"
  cargo test -p almanac-core --locked --no-run
fi

log "Done. Verify with: cargo test -p almanac-core --locked && npm run build"
