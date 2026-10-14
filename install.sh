#!/usr/bin/env bash
# One-command installer for IDE Levity on Linux & macOS.
# Installs `ide-desktop` and the `levity` CLI alias (like `code .` in VS Code),
# plus a desktop application entry and default `~/.config/ide-levity/ai.json` template.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_SRC="${SCRIPT_DIR}/target/release/ide-desktop"

FORCE_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --build|-b) FORCE_BUILD=1 ;;
  esac
done

if [[ ! -x "${BIN_SRC}" || "${FORCE_BUILD}" -eq 1 ]]; then
  echo "[1/3] Compiling IDE Levity release binary..."
  cargo build --manifest-path "${SCRIPT_DIR}/Cargo.toml" --release -p ide-desktop
else
  echo "[1/3] Using existing release binary at ${BIN_SRC}"
fi

if [[ "${EUID:-$(id -u)}" -eq 0 && -w "/usr/local/bin" ]]; then
  INSTALL_BIN_DIR="/usr/local/bin"
  DESKTOP_DIR="/usr/share/applications"
else
  INSTALL_BIN_DIR="${HOME}/.local/bin"
  DESKTOP_DIR="${HOME}/.local/share/applications"
fi

mkdir -p "${INSTALL_BIN_DIR}"
echo "[2/3] Installing binaries to ${INSTALL_BIN_DIR}..."
install -m 0755 "${BIN_SRC}" "${INSTALL_BIN_DIR}/ide-desktop"
ln -sf "${INSTALL_BIN_DIR}/ide-desktop" "${INSTALL_BIN_DIR}/levity"
ln -sf "${INSTALL_BIN_DIR}/ide-desktop" "${INSTALL_BIN_DIR}/ide-levity"

if [[ "$(uname -s)" == "Linux" ]]; then
  mkdir -p "${DESKTOP_DIR}"
  cat > "${DESKTOP_DIR}/ide-levity.desktop" <<EOF
[Desktop Entry]
Version=1.0
Type=Application
Name=IDE Levity
GenericName=Code Editor & AI IDE
Comment=High-performance Rust & AI Agentic IDE
Exec=${INSTALL_BIN_DIR}/levity %F
Terminal=false
Categories=Development;IDE;TextEditor;
Keywords=ide;editor;rust;ai;levity;
StartupNotify=true
EOF
fi

CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/ide-levity"
mkdir -p "${CONFIG_DIR}"
if [[ ! -f "${CONFIG_DIR}/ai.json.example" ]]; then
  cat > "${CONFIG_DIR}/ai.json.example" <<'EOF'
{
  "provider": "openai",
  "model": "gpt-4o",
  "api_key_env": "OPENAI_API_KEY"
}
EOF
fi

echo "[3/3] Installation complete!"
echo "  - Run GUI or open a folder : levity ."
echo "  - Open a specific file     : levity path/to/file.rs"
echo "  - Terminal interactive mode: levity -i ."
echo "  - Version check            : $( "${INSTALL_BIN_DIR}/levity" --version )"
