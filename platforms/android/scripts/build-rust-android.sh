#!/usr/bin/env bash
# Cross-compiles the `ide-ffi` crate into Android JNI shared libraries (`libide_ffi.so`) for every
# supported ABI with `cargo-ndk`, then regenerates the Mozilla UniFFI Kotlin bindings
# (`uniffi/ide_ffi/ide_ffi.kt`) from the freshly built library using the workspace `uniffi-bindgen`.
#
# Usage:
#   platforms/android/scripts/build-rust-android.sh [--debug] [--abis "arm64-v8a x86_64"] [--skip-bindings]
#
# Environment:
#   ANDROID_NDK_HOME   Path to the Android NDK. If unset, it is derived from ANDROID_HOME /
#                      ANDROID_SDK_ROOT and the NDK version pinned in `gradle.properties`.
#   CARGO_TARGET_DIR   Optional custom Cargo target directory (honored for locating build outputs).
#   IDE_ANDROID_ABIS   Space separated ABI list (default: "arm64-v8a armeabi-v7a x86_64 x86").
#
# Outputs (both are build artifacts, `*.so` files are git-ignored):
#   app/src/main/jniLibs/<abi>/libide_ffi.so
#   app/src/main/java/uniffi/ide_ffi/ide_ffi.kt
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ANDROID_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${ANDROID_ROOT}/../.." && pwd)"
JNI_LIBS_DIR="${ANDROID_ROOT}/app/src/main/jniLibs"
KOTLIN_OUT_DIR="${ANDROID_ROOT}/app/src/main/java"

PROFILE="release"
SKIP_BINDINGS=0
ABIS="${IDE_ANDROID_ABIS:-arm64-v8a armeabi-v7a x86_64 x86}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug) PROFILE="debug"; shift ;;
    --release) PROFILE="release"; shift ;;
    --abis) ABIS="$2"; shift 2 ;;
    --skip-bindings) SKIP_BINDINGS=1; shift ;;
    -h|--help) sed -n '2,19p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

# Gradle invocations from IDEs often run without the rustup shims on PATH.
if ! command -v cargo >/dev/null 2>&1 && [[ -x "${HOME}/.cargo/bin/cargo" ]]; then
  export PATH="${HOME}/.cargo/bin:${PATH}"
fi
command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found on PATH" >&2; exit 1; }
if ! cargo ndk --help >/dev/null 2>&1; then
  echo "error: cargo-ndk is not installed. Run: cargo install cargo-ndk --locked" >&2
  exit 1
fi

# Read a `key=value` entry from gradle.properties (single source of truth for versions).
gradle_prop() {
  sed -n "s/^$1=//p" "${ANDROID_ROOT}/gradle.properties" | tr -d '\r' | tail -n 1
}

MIN_SDK="$(gradle_prop ide.minSdk)"
NDK_VERSION="$(gradle_prop ide.ndkVersion)"
: "${MIN_SDK:?ide.minSdk missing from gradle.properties}"
: "${NDK_VERSION:?ide.ndkVersion missing from gradle.properties}"

if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
  SDK_DIR="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
  if [[ -z "${SDK_DIR}" && -f "${ANDROID_ROOT}/local.properties" ]]; then
    SDK_DIR="$(sed -n 's/^sdk.dir=//p' "${ANDROID_ROOT}/local.properties" | tr -d '\r')"
  fi
  if [[ -n "${SDK_DIR}" && -d "${SDK_DIR}/ndk/${NDK_VERSION}" ]]; then
    export ANDROID_NDK_HOME="${SDK_DIR}/ndk/${NDK_VERSION}"
  else
    echo "error: ANDROID_NDK_HOME is unset and NDK ${NDK_VERSION} was not found under the Android SDK." >&2
    echo "       Install it with: sdkmanager \"ndk;${NDK_VERSION}\"" >&2
    exit 1
  fi
fi

abi_to_triple() {
  case "$1" in
    arm64-v8a) echo "aarch64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86_64) echo "x86_64-linux-android" ;;
    x86) echo "i686-linux-android" ;;
    *) echo "error: unsupported Android ABI '$1'" >&2; return 1 ;;
  esac
}

TARGET_ARGS=()
for abi in ${ABIS}; do
  triple="$(abi_to_triple "${abi}")"
  if command -v rustup >/dev/null 2>&1 && ! rustup target list --installed | grep -qx "${triple}"; then
    echo "Installing missing Rust target ${triple}..."
    rustup target add "${triple}"
  fi
  TARGET_ARGS+=(-t "${abi}")
done

CARGO_PROFILE_ARGS=()
[[ "${PROFILE}" == "release" ]] && CARGO_PROFILE_ARGS+=(--release)

echo "[1/2] Cross-compiling ide-ffi (${PROFILE}) for ABIs: ${ABIS} (API ${MIN_SDK}, NDK ${NDK_VERSION})"
# Remove stale libraries (e.g. ABIs that are no longer requested) before staging fresh ones.
rm -rf "${JNI_LIBS_DIR}"
mkdir -p "${JNI_LIBS_DIR}"
cargo ndk \
  --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" \
  --platform "${MIN_SDK}" \
  "${TARGET_ARGS[@]}" \
  --output-dir "${JNI_LIBS_DIR}" \
  build -p ide-ffi --lib "${CARGO_PROFILE_ARGS[@]}"

for abi in ${ABIS}; do
  so="${JNI_LIBS_DIR}/${abi}/libide_ffi.so"
  [[ -f "${so}" ]] || { echo "error: expected ${so} was not produced" >&2; exit 1; }
done

if [[ "${SKIP_BINDINGS}" -eq 1 ]]; then
  echo "[2/2] Skipping Kotlin binding generation (--skip-bindings)."
  exit 0
fi

# UniFFI library mode reads the exported metadata symbols from any ELF, so the Android library
# that is actually shipped is the source of truth for the generated bindings.
TARGET_DIR="${CARGO_TARGET_DIR:-${WORKSPACE_ROOT}/target}"
FIRST_ABI="${ABIS%% *}"
METADATA_LIB="${TARGET_DIR}/$(abi_to_triple "${FIRST_ABI}")/${PROFILE}/libide_ffi.so"
[[ -f "${METADATA_LIB}" ]] || METADATA_LIB="${JNI_LIBS_DIR}/${FIRST_ABI}/libide_ffi.so"

echo "[2/2] Generating UniFFI Kotlin bindings from ${METADATA_LIB}"
GEN_DIR="$(mktemp -d)"
trap 'rm -rf "${GEN_DIR}"' EXIT
cargo run --quiet --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --bin uniffi-bindgen -- \
  generate --library "${METADATA_LIB}" --language kotlin --no-format --out-dir "${GEN_DIR}"

GENERATED="${GEN_DIR}/uniffi/ide_ffi/ide_ffi.kt"
[[ -f "${GENERATED}" ]] || { echo "error: uniffi-bindgen did not produce ${GENERATED}" >&2; exit 1; }
mkdir -p "${KOTLIN_OUT_DIR}/uniffi/ide_ffi"
cp -f "${GENERATED}" "${KOTLIN_OUT_DIR}/uniffi/ide_ffi/ide_ffi.kt"

echo "Android Rust libraries and Kotlin bindings are up to date."
