#!/usr/bin/env bash
set -euo pipefail

source_path="${LLM_SOURCE_PATH:-}"
destination_path="${LLM_DEST_PATH:-$HOME/.warp-oss/llm.toml}"
fallback_source_one="${HOME}/Projects/personal/mistral-vibe/.vibe/config.toml"
fallback_source_two="${HOME}/.vibe/config.toml"
prompt_id="${LLM_SYSTEM_PROMPT_ID:-}"
prompt_path="${LLM_SYSTEM_PROMPT_PATH:-}"
sync_prompt="${LLM_SYNC_SYSTEM_PROMPT:-1}"
mistral_vibe_root="${MISTRAL_VIBE_ROOT:-$HOME/Projects/personal/mistral-vibe}"

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

if [[ "${sync_prompt}" != "0" ]]; then
  python3 - "$source_path" "$destination_path" "$mistral_vibe_root" "$prompt_id" "$prompt_path" <<'PY'
import sys
from pathlib import Path
import os

source_path, destination_path, root, prompt_id, prompt_path = sys.argv[1:6]

source_root = Path(root).expanduser()
dest = Path(destination_path).expanduser()
prompt_id = prompt_id.strip()
explicit_prompt_path = prompt_path.strip()

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python <3.11 fallback
    import tomli as tomllib  # type: ignore


def resolve_prompt_path(config_text: str) -> Path | None:
    data = tomllib.loads(config_text)
    resolved_prompt_id = data.get("system_prompt_id", "cli")
    if prompt_id:
        resolved_prompt_id = prompt_id

    candidates: list[Path] = []

    if explicit_prompt_path:
        candidates.append(Path(explicit_prompt_path).expanduser())

    user_home = Path.home()
    candidates.extend(
        [
            user_home / ".vibe" / "prompts" / f"{resolved_prompt_id}.md",
            source_root / ".vibe" / "prompts" / f"{resolved_prompt_id}.md",
            source_root / "vibe" / "core" / "prompts" / f"{resolved_prompt_id}.md",
        ]
    )

    if not explicit_prompt_path:
        resolved_prompt_id = resolved_prompt_id.strip().lower()

    for candidate in candidates:
        if not candidate:
            continue
        if candidate.is_file():
            return candidate

    return None


def with_agent_block(content: str, prompt_file: Path | None) -> str:
    if prompt_file is None:
        return content.rstrip() + "\n"

    prompt_file_dest = Path(os.path.expandvars(os.path.expanduser("~/.warp-oss/agent-system-prompt.md")))
    prompt_file_dest.parent.mkdir(parents=True, exist_ok=True)
    prompt_file_dest.write_text(prompt_file.read_text(encoding="utf-8"), encoding="utf-8")

    lines = content.splitlines()
    out: list[str] = []
    skipping = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if line.strip() == "[agent]":
                skipping = True
                continue
            if skipping:
                skipping = False

        if skipping:
            if stripped.startswith("[") and stripped.endswith("]"):
                skipping = False
            else:
                continue

        if not skipping:
            out.append(line)

    return (
        "\n".join(out).rstrip()
        + "\n\n"
        + "[agent]\n"
        + f'system_prompt_file = "{prompt_file_dest.as_posix()}"\n'
    )


with open(source_path, "rb") as source_file:
    source_text = source_file.read().decode("utf-8")

with open(destination_path, "r", encoding="utf-8") as dest_file:
    destination_text = dest_file.read()

prompt_source = resolve_prompt_path(source_text)
if prompt_source is None:
    print(
        "No matching system prompt file found; skipping [agent] system_prompt_file injection.",
        file=sys.stderr,
    )
else:
    new_content = with_agent_block(destination_text, prompt_source)
    with open(destination_path, "w", encoding="utf-8") as dest_file:
        dest_file.write(new_content)
    print(
        f"Injected [agent].system_prompt_file pointing at {prompt_source} into {destination_path}"
    )
PY
fi
