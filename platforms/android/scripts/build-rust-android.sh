#!/usr/bin/env bash
set -euo pipefail

# Cross-compiles `ide-ffi` into Android JNI shared libraries (`libide_ffi.so`) using `cargo-ndk`
# and generates Mozilla UniFFI Kotlin bindings (`uniffi/ide_ffi/ide_ffi.kt`).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ANDROID_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${ANDROID_ROOT}/../.." && pwd)"
JNI_LIBS_DIR="${ANDROID_ROOT}/app/src/main/jniLibs"
KOTLIN_OUT_DIR="${ANDROID_ROOT}/app/src/main/java"

mkdir -p "${JNI_LIBS_DIR}" "${KOTLIN_OUT_DIR}"

echo "[1/3] Building host libide_ffi for UniFFI bindgen..."
cargo build --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --release

HOST_LIB=""
for ext in so dylib dll; do
  if [[ -f "${WORKSPACE_ROOT}/target/release/libide_ffi.${ext}" ]]; then
    HOST_LIB="${WORKSPACE_ROOT}/target/release/libide_ffi.${ext}"
    break
  elif [[ -f "${WORKSPACE_ROOT}/target/release/ide_ffi.${ext}" ]]; then
    HOST_LIB="${WORKSPACE_ROOT}/target/release/ide_ffi.${ext}"
    break
  fi
done

echo "[2/3] Generating UniFFI Kotlin bindings into ${KOTLIN_OUT_DIR}..."
cargo run --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --bin uniffi-bindgen -- \
  generate --library "${HOST_LIB}" --language kotlin --out-dir "${KOTLIN_OUT_DIR}"

if command -v cargo-ndk >/dev/null 2>&1 && [[ -n "${ANDROID_NDK_HOME:-}" ]]; then
  echo "[3/3] Cross-compiling Android ABIs via cargo-ndk..."
  cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    -o "${JNI_LIBS_DIR}" \
    build --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --release
else
  echo "[3/3] Staging host shared library into ${JNI_LIBS_DIR}/x86_64/libide_ffi.so..."
  mkdir -p "${JNI_LIBS_DIR}/x86_64"
  if [[ -f "${WORKSPACE_ROOT}/target/release/libide_ffi.so" ]]; then
    cp -f "${WORKSPACE_ROOT}/target/release/libide_ffi.so" "${JNI_LIBS_DIR}/x86_64/libide_ffi.so"
  fi
fi

echo "Android Rust FFI build & binding generation complete."
