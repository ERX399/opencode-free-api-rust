#!/bin/bash
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rm -rf packages
mkdir -p packages

README_TMPL='OpenCode Free API - single-file proxy for free models
===============================================

How to start (no install, no dependencies, no scripts):
  Windows:  double-click  opencode-free-api.exe
  Linux:    ./opencode-free-api
  macOS:    chmod +x opencode-free-api && ./opencode-free-api
  Android:  copy the binary to Termux (or root), then run it directly

The console shows the listening addresses (local / LAN / public).
If the port is already in use the program prints a friendly message
instead of crashing, then exits after a few seconds.

After starting, point any OpenAI-compatible client at:
  Base URL:  http://127.0.0.1:8788/v1  (or the LAN/public address shown)
  API key:   anything (empty is fine)

Optional configuration (environment variables, e.g. in PowerShell
  $env:PORT=9000 then run the exe, or set system variables):
  PORT          port to listen on (default 8788)
  NODES         comma-separated upstream base URLs
  API_TOKEN     bearer token sent to each upstream
  AUTH_TOKEN    if set, clients must send Authorization: Bearer <value>
  STRIP_FREE    strip the -free suffix from model ids and map calls back
  UPSTREAM_TIMEOUT  non-streaming upstream timeout seconds (default 90)
  STREAM_TIMEOUT    streaming upstream timeout seconds (default 1800)
  CONNECT_TIMEOUT   upstream connect timeout seconds (default 10)
  WORKERS           concurrent worker threads (default = CPU cores)
  HOST              bind address (default 0.0.0.0)

Default upstream is the official OpenCode Zen gateway and only free
models are exposed.
'

pack() {
  plat="$1"; bin="$2"; target_name="$3"
  dir="opencode-free-api-$plat"
  out="packages/$dir"
  mkdir -p "$out"
  cp "dist/$bin" "$out/$target_name"
  chmod +x "$out/$target_name"
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

pack linux-x86_64 opencode-free-api-linux-x86_64 opencode-free-api
pack linux-aarch64 opencode-free-api-linux-aarch64 opencode-free-api
pack windows-x86_64 opencode-free-api-windows-x86_64.exe opencode-free-api.exe
pack macos-x86_64 opencode-free-api-macos-x86_64 opencode-free-api
pack macos-arm64 opencode-free-api-macos-arm64 opencode-free-api
pack android-arm64 opencode-free-api-android-arm64 opencode-free-api
pack android-x86_64 opencode-free-api-android-x86_64 opencode-free-api

find packages -type f | sort
