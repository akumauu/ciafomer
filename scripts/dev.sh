#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

echo "=== Ciallo Dev Setup ==="

# 1. Install Node dependencies
echo "[1/4] Installing Node dependencies..."
cd "$PROJECT_ROOT"
npm install

# 2. Build frontend
echo "[2/4] Building frontend..."
npm run build

# 3. Create Python venv (optional, for OCR worker)
if [ ! -d "$PROJECT_ROOT/python-worker/.venv" ]; then
    echo "[3/4] Creating Python virtual environment..."
    python3 -m venv "$PROJECT_ROOT/python-worker/.venv"
    source "$PROJECT_ROOT/python-worker/.venv/bin/activate"
    pip install --upgrade pip
    # Only install msgpack for Phase 1 (OCR deps are heavy)
    pip install msgpack
    deactivate
else
    echo "[3/4] Python venv already exists, skipping."
fi

# 4. Build Rust backend
echo "[4/4] Building Rust backend (debug)..."
cd "$PROJECT_ROOT/src-tauri"
cargo build 2>&1

echo ""
echo "=== Setup complete ==="
echo "Run: cd $PROJECT_ROOT/src-tauri && cargo run"
