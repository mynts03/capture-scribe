#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

swift build -c release --package-path "$project_dir/native/macos-capture-helper"
cp \
  "$project_dir/native/macos-capture-helper/.build/release/capture-scribe-capture" \
  "$project_dir/native/macos-capture-helper/.build/release/capture-scribe-capture-aarch64-apple-darwin"

swift build -c release --package-path "$project_dir/native/apple-speech-transcriber"
