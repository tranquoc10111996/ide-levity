#!/usr/bin/env bash
set -euo pipefail

# Builds `ide-ffi` for iOS Device (aarch64-apple-ios) and iOS Simulator (aarch64-apple-ios-sim, x86_64-apple-ios),
# generates Swift bindings via `uniffi-bindgen`, and packages `IdeFfi.xcframework`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IOS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${IOS_ROOT}/../.." && pwd)"
SWIFT_GEN_DIR="${IOS_ROOT}/Sources/IDELevityiOS/Generated"
C_HEADER_DIR="${IOS_ROOT}/Sources/ide_ffiFFI"
XCFRAMEWORK_OUT="${IOS_ROOT}/Frameworks/IdeFfi.xcframework"

mkdir -p "${SWIFT_GEN_DIR}" "${C_HEADER_DIR}" "${IOS_ROOT}/Frameworks"

echo "[1/3] Building host libide_ffi for UniFFI Swift bindgen..."
cargo build --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --release

HOST_LIB=""
for ext in dylib so dll; do
  if [[ -f "${WORKSPACE_ROOT}/target/release/libide_ffi.${ext}" ]]; then
    HOST_LIB="${WORKSPACE_ROOT}/target/release/libide_ffi.${ext}"
    break
  fi
done

echo "[2/3] Generating UniFFI Swift bindings..."
TMP_OUT="$(mktemp -d)"
cargo run --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --bin uniffi-bindgen -- \
  generate --library "${HOST_LIB}" --language swift --out-dir "${TMP_OUT}"

cp -f "${TMP_OUT}/ide_ffi.swift" "${SWIFT_GEN_DIR}/ide_ffi.swift"
cp -f "${TMP_OUT}/ide_ffiFFI.h" "${C_HEADER_DIR}/ide_ffiFFI.h"
cp -f "${TMP_OUT}/ide_ffiFFI.modulemap" "${C_HEADER_DIR}/module.modulemap"
rm -rf "${TMP_OUT}"

if [[ "$(uname -s)" == "Darwin" ]] && command -v xcodebuild >/dev/null 2>&1; then
  echo "[3/3] Cross-compiling iOS targets and assembling IdeFfi.xcframework..."
  rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
  cargo build --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --release --target aarch64-apple-ios
  cargo build --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" -p ide-ffi --release --target aarch64-apple-ios-sim

  rm -rf "${XCFRAMEWORK_OUT}"
  xcodebuild -create-xcframework \
    -library "${WORKSPACE_ROOT}/target/aarch64-apple-ios/release/libide_ffi.a" \
    -headers "${C_HEADER_DIR}" \
    -library "${WORKSPACE_ROOT}/target/aarch64-apple-ios-sim/release/libide_ffi.a" \
    -headers "${C_HEADER_DIR}" \
    -output "${XCFRAMEWORK_OUT}"
else
  echo "[3/3] Non-macOS host detected; Swift bindings and headers staged in ${IOS_ROOT}/Sources."
fi

echo "iOS UniFFI Swift binding & XCFramework step completed."
