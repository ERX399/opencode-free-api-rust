#!/bin/bash
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rm -rf packages
mkdir -p packages

BAT_TMPL='@echo off
setlocal
cd /d %~dp0
set HOST=0.0.0.0
set PORT=8788
set STRIP_FREE=1
rem Optional: send a token to the upstream (e.g. your Zen key)
rem set API_TOKEN=your-zen-key
rem Optional: require clients to send Authorization: Bearer <value>
rem set AUTH_TOKEN=my-secret
echo OpenCode Free API proxy
echo Listening on  http://localhost:%PORT%
echo API endpoint: http://localhost:%PORT%/v1/chat/completions
%~dp0__BIN__
pause
'

SH_TMPL='#!/bin/sh
cd "$(dirname "$0")" || exit 1
HOST=${HOST:-0.0.0.0}
PORT=${PORT:-8788}
export HOST PORT STRIP_FREE=1
# Optional: send a token to the upstream (e.g. your Zen key)
# export API_TOKEN="your-zen-key"
# Optional: require clients to send Authorization: Bearer <value>
# export AUTH_TOKEN="my-secret"
echo "OpenCode Free API proxy"
echo "Listening on   http://localhost:$PORT"
echo "API endpoint:  http://localhost:$PORT/v1/chat/completions"
exec "./__BIN__"
'

README_TMPL='OpenCode Free API - single-file proxy for free models
===============================================

How to start (no install, no dependencies):
  Windows:  double-click  start.bat
  Linux:    ./start.sh
  macOS:    chmod +x start.command && ./start.command

After starting, point any OpenAI-compatible client at:
  Base URL:  http://localhost:8788/v1
  API key:   anything (empty is fine)

Default upstream is the official OpenCode Zen gateway and only free
models are exposed. Edit the start script to set API_TOKEN / AUTH_TOKEN
or a custom NODES list.
'

pack() {
  plat="$1"; bin="$2"; script="$3"
  dir="opencode-free-api-$plat"
  out="packages/$dir"
  mkdir -p "$out"
  cp "dist/$bin" "$out/$bin"
  chmod +x "$out/$bin"

  case "$script" in
    *.bat)
      printf '%s' "$BAT_TMPL" | sed "s|__BIN__|$bin|" | sed 's/$/\r/' > "$out/start.bat"
      ;;
    *)
      printf '%s' "$SH_TMPL" | sed "s|__BIN__|$bin|" > "$out/$script"
      chmod +x "$out/$script"
      ;;
  esac

  printf '%s' "$README_TMPL" > "$out/README.txt"

  case "$plat" in
    windows*)
      (cd packages && zip -q -r "$dir.zip" "$dir")
      ;;
    *)
      tar -czf "packages/$dir.tar.gz" -C packages "$dir"
      ;;
  esac
  rm -rf "$out"
  echo "packed $dir"
}

pack linux-x86_64 opencode-free-api-linux-x86_64 start.sh
pack linux-aarch64 opencode-free-api-linux-aarch64 start.sh
pack windows-x86_64 opencode-free-api-windows-x86_64.exe start.bat
pack macos-x86_64 opencode-free-api-macos-x86_64 start.command
pack macos-arm64 opencode-free-api-macos-arm64 start.command

find packages -type f | sort
