#!/usr/bin/env bash
set -euo pipefail
# Build the bingle_jsi Rust crate for Android (arm64, armv7, x86_64) and generate
# Kotlin bindings via uniffi-bindgen, then copy shared libraries into jniLibs.
#
# Prerequisites:
#   - Android NDK (set ANDROID_NDK_HOME or auto-detected from ANDROID_HOME)
#   - rustup with Android targets (installed automatically if missing)
#   - uniffi-bindgen is built from the bingle_jsi crate (no separate install needed)
#
# Usage:
#   bash bingle_jsi/scripts/build_android.sh
#   BINGLE_ANDROID_ABIS=x86_64 bash bingle_jsi/scripts/build_android.sh   # one ABI (fast, for tests)
#
# BINGLE_ANDROID_ABIS: comma-separated ABIs to build (default: all — arm64-v8a,armeabi-v7a,x86_64).
# For a fast test/e2e build, restrict to just the emulator's ABI (like the iOS BINGLE_IOS_SIM_ONLY
# build): arm64-v8a on an Apple-silicon emulator, x86_64 on a Linux CI emulator.
#
# Output:
#   bingle_jsi/android/src/main/jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/libbingle_jsi.so
#   bingle_jsi/android/generated/                                       -- Kotlin bindings

CRATE_NAME="bingle_jsi"
ROOT_DIR="$(cd "$(dirname "$0")"/../.. && pwd)"
JSI_DIR="$ROOT_DIR/bingle_jsi"
ANDROID_DIR="$JSI_DIR/android"
JNILIBS_DIR="$ANDROID_DIR/src/main/jniLibs"
# Build into a username-free target dir. Vendored OpenSSL bakes its
# OPENSSLDIR/ENGINESDIR/MODULESDIR constants from OUT_DIR at C-compile time;
# --remap-path-prefix (a rustc flag) cannot rewrite those, so if OUT_DIR sat
# under $ROOT_DIR the builder's home path would leak into the shipped .so.
# Overridable by setting CARGO_TARGET_DIR in the environment.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/var/tmp/bingle_native_target}"
BUILD_DIR="$CARGO_TARGET_DIR"
GENERATED_DIR="$ANDROID_DIR/generated"

# ── helpers ───────────────────────────────────────────────────────────

have_cmd() { command -v "$1" >/dev/null 2>&1; }

require_cmd() {
  if ! have_cmd "$1"; then
    echo "Error: '$1' not found. Please install it and try again." >&2
    exit 1
  fi
}

ensure_target() {
  local target="$1"
  local tc="${ACTIVE_TOOLCHAIN:-}"
  local list_cmd=(rustup target list --installed)
  local add_cmd=(rustup target add)
  if [[ -n "$tc" ]]; then
    list_cmd=(rustup target list --installed --toolchain "$tc")
    add_cmd=(rustup target add --toolchain "$tc")
  fi
  if "${list_cmd[@]}" | grep -qx "$target"; then
    return 0
  fi
  echo "Installing Rust target: $target"
  "${add_cmd[@]}" "$target"
}

require_cmd rustup
require_cmd cargo

ACTIVE_TOOLCHAIN="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
if [[ -z "${ACTIVE_TOOLCHAIN:-}" ]]; then
  echo "Error: Could not determine active rustup toolchain." >&2
  exit 1
fi

CARGO_CMD=(rustup run "$ACTIVE_TOOLCHAIN" cargo)
if [[ -n "${BORINGCACHE_CARGO_POLICY:-}" ]]; then
  case "$BORINGCACHE_CARGO_POLICY" in
    write|read-only) ;;
    *) echo "Invalid BoringCache Cargo policy" >&2; exit 2 ;;
  esac
  CARGO_CMD=(rustup run "$ACTIVE_TOOLCHAIN" boringcache cargo --"$BORINGCACHE_CARGO_POLICY" --profile android)
fi

# ── strip local build paths from the shipped library ──────────────────
# Dependency panic-location strings and debuginfo embed absolute source
# paths, mostly under $CARGO_HOME (~/.cargo/registry). Left as-is these
# leak the builder's username and filesystem layout into the .so files
# bundled in the npm package. Remap them to stable placeholders. Computed
# from the live environment so no personal path is committed to the repo.
# (Android has no per-target rustflags in .cargo/config.toml, so exporting
# RUSTFLAGS here does not clobber anything.)
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup --remap-path-prefix=$ROOT_DIR=/bingle"

# ── locate Android NDK ────────────────────────────────────────────────

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  # Try to auto-detect from ANDROID_HOME
  if [[ -n "${ANDROID_HOME:-}" ]] && [[ -d "$ANDROID_HOME/ndk" ]]; then
    # Pick the latest installed NDK
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME/ndk/"* 2>/dev/null | sort -V | tail -1)"
  elif [[ -d "$HOME/Library/Android/sdk/ndk" ]]; then
    ANDROID_NDK_HOME="$(ls -d "$HOME/Library/Android/sdk/ndk/"* 2>/dev/null | sort -V | tail -1)"
  fi
fi

if [[ -z "${ANDROID_NDK_HOME:-}" ]] || [[ ! -d "${ANDROID_NDK_HOME}" ]]; then
  echo "Error: Android NDK not found." >&2
  echo "Set ANDROID_NDK_HOME or install via Android Studio SDK Manager." >&2
  exit 1
fi

echo "Using Android NDK: $ANDROID_NDK_HOME"

# ── configure cargo for cross-compilation ─────────────────────────────

# Determine NDK toolchain bin directory
NDK_TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt"
HOST_TAG=""
if [[ "$(uname)" == "Darwin" ]]; then
  HOST_TAG="darwin-x86_64"
elif [[ "$(uname)" == "Linux" ]]; then
  HOST_TAG="linux-x86_64"
fi

NDK_BIN="$NDK_TOOLCHAIN/$HOST_TAG/bin"
if [[ ! -d "$NDK_BIN" ]]; then
  echo "Error: NDK toolchain bin not found at $NDK_BIN" >&2
  exit 1
fi

# Set the minimum API level
API_LEVEL=21

# Put the NDK toolchain on PATH so the C compiler can be invoked by bare name
# below. Vendored OpenSSL bakes the CC command verbatim into a compiled-in
# build-info string (OPENSSL_CFLAGS, in .rodata — read at runtime, so neither
# --remap-path-prefix nor `strip` can remove it). Passing CC as an absolute
# path would embed the builder's home dir there; a bare name keeps it clean.
export PATH="$NDK_BIN:$PATH"

# Export linker environment variables for each target
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android${API_LEVEL}-clang"
export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$NDK_BIN/armv7a-linux-androideabi${API_LEVEL}-clang"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK_BIN/x86_64-linux-android${API_LEVEL}-clang"

# Also set CC for openssl-sys (vendored). Bare tool names (resolved via the
# PATH entry above) keep the NDK's absolute path out of OpenSSL's baked-in
# build-info string — see scripts/scan_native_leaks.sh.
export CC_aarch64_linux_android="aarch64-linux-android${API_LEVEL}-clang"
export CC_armv7_linux_androideabi="armv7a-linux-androideabi${API_LEVEL}-clang"
export CC_x86_64_linux_android="x86_64-linux-android${API_LEVEL}-clang"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
export AR_armv7_linux_androideabi="$NDK_BIN/llvm-ar"
export AR_x86_64_linux_android="$NDK_BIN/llvm-ar"
export RANLIB_aarch64_linux_android="$NDK_BIN/llvm-ranlib"
export RANLIB_armv7_linux_androideabi="$NDK_BIN/llvm-ranlib"
export RANLIB_x86_64_linux_android="$NDK_BIN/llvm-ranlib"

# ── Android target triples ────────────────────────────────────────────

declare -A ABI_TO_TARGET=(
  ["arm64-v8a"]="aarch64-linux-android"
  ["armeabi-v7a"]="armv7-linux-androideabi"
  ["x86_64"]="x86_64-linux-android"
)

# Which ABIs to build (default: all). BINGLE_ANDROID_ABIS restricts to a subset for fast builds.
if [[ -n "${BINGLE_ANDROID_ABIS:-}" ]]; then
  IFS=',' read -r -a ABIS <<< "$BINGLE_ANDROID_ABIS"
else
  ABIS=(arm64-v8a armeabi-v7a x86_64)
fi

# ── install targets and build ─────────────────────────────────────────

BINDING_TARGET=""
for abi in "${ABIS[@]}"; do
  target="${ABI_TO_TARGET[$abi]:-}"
  if [[ -z "$target" ]]; then
    echo "Error: unknown Android ABI '$abi' (expected: arm64-v8a, armeabi-v7a, x86_64)" >&2
    exit 1
  fi
  ensure_target "$target"

  echo "Building $CRATE_NAME for $target ($abi)..."
  "${CARGO_CMD[@]}" build -p "$CRATE_NAME" --lib --release --target "$target"

  # Copy .so to jniLibs
  SO_FILE="$BUILD_DIR/$target/release/lib${CRATE_NAME}.so"
  if [[ ! -f "$SO_FILE" ]]; then
    echo "Error: Missing shared library: $SO_FILE" >&2
    exit 1
  fi

  mkdir -p "$JNILIBS_DIR/$abi"
  cp "$SO_FILE" "$JNILIBS_DIR/$abi/lib${CRATE_NAME}.so"
  echo "  -> $JNILIBS_DIR/$abi/lib${CRATE_NAME}.so"
  BINDING_TARGET="$target"
done

# ── generate Kotlin bindings via uniffi-bindgen ───────────────────────

echo "Generating Kotlin bindings..."
mkdir -p "$GENERATED_DIR"

# Use any built library for binding generation (all ABIs share the same interface).
BINDING_LIB="$BUILD_DIR/$BINDING_TARGET/release/lib${CRATE_NAME}.so"

"${CARGO_CMD[@]}" run -p bingle_jsi --bin uniffi-bindgen -- generate \
  --library "$BINDING_LIB" \
  --language kotlin \
  --out-dir "$GENERATED_DIR"

echo "Kotlin bindings generated in $GENERATED_DIR"

# ── guard: fail if any build-machine path leaked into the .so files ───
source "$(dirname "$0")/../../scripts/scan_native_leaks.sh"
scan_native_leaks "$JNILIBS_DIR"

echo ""
echo "=== Android build complete ==="
echo "jniLibs: $JNILIBS_DIR/"
echo "Kotlin bindings: $GENERATED_DIR/"
echo ""
echo "To use in a React Native project:"
echo "  1. Add this module as an npm dependency (see package.json)"
echo "  2. Run 'npx react-native run-android'"
echo "  3. The shared libraries and Kotlin bindings are linked via build.gradle"
