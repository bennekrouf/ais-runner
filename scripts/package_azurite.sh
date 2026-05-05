#!/bin/bash
set -e

# This script packages Azurite as a standalone binary for the current platform.
# It uses 'pkg' to bundle the Node.js runtime and Azurite code.

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
BIN_DIR="$PROJECT_ROOT/bin"
TEMP_DIR="$PROJECT_ROOT/tmp_azurite_build"

mkdir -p "$BIN_DIR"
mkdir -p "$TEMP_DIR"

echo "📦 Packaging Azurite into standalone binary..."

# 1. Ensure pkg is available
if ! command -v pkg &> /dev/null; then
    echo "⚠️ 'pkg' not found. Installing globally..."
    npm install -g pkg
fi

# 2. Install Azurite locally in temp dir
cd "$TEMP_DIR"
if [ ! -d "node_modules/azurite" ]; then
    echo "⬇️ Downloading Azurite..."
    npm init -y > /dev/null
    npm install azurite@3.35.0 --no-save
fi

# 3. Determine target platform
# targets: node18-macos-x64, node18-macos-arm64, node18-win-x64, node18-linux-x64
OS="$(uname -s)"
ARCH="$(uname -m)"
TARGET=""

if [ "$OS" == "Darwin" ]; then
    if [ "$ARCH" == "arm64" ]; then
        TARGET="node18-macos-arm64"
    else
        TARGET="node18-macos-x64"
    fi
    OUTPUT="$BIN_DIR/azurite"
elif [ "$OS" == "Linux" ]; then
    TARGET="node18-linux-x64"
    OUTPUT="$BIN_DIR/azurite"
else
    TARGET="node18-win-x64"
    OUTPUT="$BIN_DIR/azurite.exe"
fi

echo "🚀 Building for $TARGET..."

# 4. Package it
# Entry point for Azurite is usually dist/src/azurite.js
# We use --public to include all files in the executable
pkg node_modules/azurite/dist/src/azurite.js \
    --targets "$TARGET" \
    --output "$OUTPUT" \
    --public

echo "✅ Success! Standalone Azurite created at: $OUTPUT"
echo "💡 You can now ship this file without requiring Node.js on the target machine."

# Optional: Cleanup
# rm -rf "$TEMP_DIR"
