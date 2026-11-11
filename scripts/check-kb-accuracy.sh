#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Uses an LLM API to verify that a PR's code changes don't contradict the
# knowledge base documentation. Exits 1 if contradictions are found.
# Exits 2 when the check could not run (no key, no diff, API error) so
# callers can distinguish "checked and passed" from "did not check".
#
# Required environment:
#   KB_API_KEY         — API subscription key for the LLM endpoint
#
# Optional:
#   KB_API_URL         — API endpoint (default: AMD internal LLM API)
#   KB_MODEL           — model to use (default: GPT-oss-120B)
#   KB_BASE_REF        — base ref for the diff (default: origin/main)
#   KB_MAX_DIFF_CHARS  — skip if diff is too large (default: 80000)
#
# Usage:
#   scripts/check-kb-accuracy.sh              # check current branch vs main
#   KB_BASE_REF=HEAD~1 scripts/check-kb-accuracy.sh  # check last commit

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BASE_REF="${KB_BASE_REF:-origin/main}"
MODEL="${KB_MODEL:-GPT-oss-120B}"
MAX_DIFF_CHARS="${KB_MAX_DIFF_CHARS:-80000}"
API_URL="${KB_API_URL:-https://llm-api.amd.com/OnPrem/chat/completions}"
API_KEY="${KB_API_KEY:-}"

if [ -z "$API_KEY" ]; then
    echo "::error::KB_API_KEY not set"
    exit 1
fi

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

git diff "$BASE_REF"..HEAD -- ':(glob)crates/*/src/**' ':(glob)proto/**' > "$tmpdir/diff.txt" 2>/dev/null || true
if [ ! -s "$tmpdir/diff.txt" ]; then
    echo "No source changes — skipping KB accuracy check"
    exit 0
fi

diff_chars=$(wc -c < "$tmpdir/diff.txt")
if [ "$diff_chars" -gt "$MAX_DIFF_CHARS" ]; then
    echo "Diff too large (${diff_chars} chars > ${MAX_DIFF_CHARS}) — skipping"
    exit 0
fi

changed_crates=$(git diff "$BASE_REF"..HEAD --name-only -- ':(glob)crates/*/src/**' \
    | sed -n 's|^\(crates/[^/]*/\).*|\1|p' \
    | sort -u)

: > "$tmpdir/kb.txt"
for crate_dir in $changed_crates; do
    agents_file="${crate_dir}AGENTS.md"
    if [ -f "$agents_file" ]; then
        printf '\n--- %s ---\n' "$agents_file" >> "$tmpdir/kb.txt"
        cat "$agents_file" >> "$tmpdir/kb.txt"
    fi
done

for doc in docs/knowledge/common/*.md docs/knowledge/scheduler/*.md docs/knowledge/execution/*.md docs/knowledge/consensus/*.md docs/knowledge/accounting/*.md docs/knowledge/networking/*.md docs/knowledge/security/*.md docs/knowledge/devices/*.md docs/knowledge/testing/*.md docs/knowledge/deployment/*.md; do
    [ -f "$doc" ] || continue
    printf '\n--- %s ---\n' "$doc" >> "$tmpdir/kb.txt"
    cat "$doc" >> "$tmpdir/kb.txt"
done

kb_chars=$(wc -c < "$tmpdir/kb.txt")
total_chars=$((diff_chars + kb_chars))
if [ "$total_chars" -gt 500000 ]; then
    echo "Combined context too large (${total_chars} chars) — skipping"
    exit 0
fi

python3 - "$tmpdir" "$MODEL" "$API_KEY" "$API_URL" <<'PYEOF'
import sys, json, urllib.request, urllib.error, os

tmpdir, model, api_key, api_url = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

with open(os.path.join(tmpdir, "diff.txt")) as f:
    diff_text = f.read()
with open(os.path.join(tmpdir, "kb.txt")) as f:
    kb_text = f.read()

prompt = f"""You are a documentation accuracy checker for the Spur project.

Below is a code diff from a pull request, followed by the project's knowledge base documentation.

Your task: identify any claims in the knowledge base docs that the code diff CONTRADICTS. A contradiction means the doc states something as true that the code change makes false, or the doc describes behavior that the code change breaks.

Rules:
- Only flag CONTRADICTIONS — claims the diff makes wrong.
- Do NOT flag omissions — the KB is a curated summary, not exhaustive docs.
- Do NOT flag things the diff adds that aren't yet documented.
- Do NOT flag style, formatting, or wording preferences.
- Each contradiction must cite the specific doc file, the specific claim, and the specific code change that contradicts it.

If you find NO contradictions, respond with exactly: PASS

If you find contradictions, respond with:
FAIL
Then list each contradiction as:
- [file.md:line-area] "quoted claim" — contradicted by: explanation of what the code actually does

--- CODE DIFF ---
{diff_text}

--- KNOWLEDGE BASE ---
{kb_text}"""

body = json.dumps({
    "model": model,
    "max_tokens": 4096,
    "messages": [{"role": "user", "content": prompt}],
}).encode()

headers = {
    "content-type": "application/json",
    "Ocp-Apim-Subscription-Key": api_key,
    "api-key": "dummy",
}

try:
    user = os.environ.get("RUNNER_NAME", "gh-actions-runner")
    headers["user"] = user
except Exception:
    headers["user"] = "gh-actions-runner"

req = urllib.request.Request(api_url, data=body, headers=headers)

try:
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.load(resp)
except urllib.error.HTTPError as e:
    print(f"LLM API returned HTTP {e.code}")
    error_body = e.read().decode()[:500]
    print(error_body)
    sys.exit(2)
except Exception as e:
    print(f"LLM API request failed: {e}")
    sys.exit(2)

result = ""
if "choices" in data:
    result = data["choices"][0]["message"]["content"]
elif "content" in data:
    for block in data["content"]:
        if block.get("type") == "text":
            result = block["text"]
            break

print()
print("=== Knowledge Base Accuracy Check ===")
print()

if result.strip().startswith("PASS"):
    print("PASS — no contradictions found")
    sys.exit(0)
else:
    print(result)
    print()
    print("Knowledge base has claims that contradict the code changes.")
    print("Update the relevant AGENTS.md or docs/knowledge/ files.")
    sys.exit(1)
PYEOF
