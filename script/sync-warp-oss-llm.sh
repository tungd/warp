#!/usr/bin/env bash
set -euo pipefail

source_path="${LLM_SOURCE_PATH:-}"
destination_path="${LLM_DEST_PATH:-$HOME/.warp-oss/llm.toml}"
fallback_source_one="${HOME}/Projects/personal/mistral-vibe/.vibe/config.toml"
fallback_source_two="${HOME}/.vibe/config.toml"

if [[ -z "${source_path}" ]]; then
  if [[ -f "${fallback_source_one}" ]]; then
    source_path="${fallback_source_one}"
  else
    source_path="${fallback_source_two}"
  fi
fi

if [[ ! -f "${source_path}" ]]; then
  echo "No source llm config found at: ${source_path}" >&2
  exit 1
fi

mkdir -p "$(dirname "${destination_path}")"
cp "${source_path}" "${destination_path}"
echo "Copied ${source_path} -> ${destination_path}"
