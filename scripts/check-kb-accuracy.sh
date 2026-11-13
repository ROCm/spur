#!/bin/bash

# Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Uses an LLM API to verify that a PR's code changes don't contradict the
# knowledge base documentation.
#   Exit 0: checked, no contradictions found.
#   Exit 1: checked, contradictions found.
#   Exit 2: could not run (no key, bad ref, API error, oversized input,
#            or malformed LLM verdict).
#
# Required environment:
#   KB_API_KEY         — API subscription key for the LLM endpoint
#
# Optional:
#   KB_API_URL         — API endpoint (default: AMD internal LLM API)
#   KB_MODEL           — model to use (default: Claude-Opus-4.8)
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
MODEL="${KB_MODEL:-Claude-Opus-4.8}"
MAX_DIFF_CHARS="${KB_MAX_DIFF_CHARS:-80000}"
API_URL="${KB_API_URL:-https://llm-api.amd.com/Anthropic/v1/messages}"
API_KEY="${KB_API_KEY:-}"

if [ -z "$API_KEY" ]; then
    echo "::error::KB_API_KEY not set — cannot run accuracy check"
    exit 2
fi

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

if ! git diff "$BASE_REF"..HEAD -- ':(glob)crates/*/src/**' ':(glob)proto/**' > "$tmpdir/diff.txt" 2>"$tmpdir/diff_err.txt"; then
    echo "::error::git diff failed (bad BASE_REF? missing history?)"
    cat "$tmpdir/diff_err.txt" >&2
    exit 2
fi
if [ ! -s "$tmpdir/diff.txt" ]; then
    echo "No source changes — skipping KB accuracy check"
    exit 0
fi

diff_chars=$(wc -c < "$tmpdir/diff.txt")
if [ "$diff_chars" -gt "$MAX_DIFF_CHARS" ]; then
    echo "::warning::Diff too large (${diff_chars} chars > ${MAX_DIFF_CHARS}) — accuracy check not run"
    exit 2
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
    echo "::warning::Combined context too large (${total_chars} chars) — accuracy check not run"
    exit 2
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

CRITICAL: Your response MUST begin with exactly PASS or FAIL as the very first word on the very first line. No preamble, no analysis before the verdict, no "I'll analyze..." or "Let me check..." — just the verdict first.

If you find NO contradictions, your entire response must be exactly: PASS

If you find contradictions, respond with FAIL on the first line, then list each contradiction as:
- [file.md:line-area] "quoted claim" — contradicted by: explanation of what the code actually does

--- CODE DIFF ---
{diff_text}

--- KNOWLEDGE BASE ---
{kb_text}"""

body = json.dumps({
    "model": model,
    "max_tokens": 4096,
    "system": "You are a CI checker that outputs structured verdicts. Your response MUST start with PASS or FAIL on the first line. Never add preamble.",
    "messages": [{"role": "user", "content": prompt}],
}).encode()

headers = {
    "content-type": "application/json",
    "anthropic-version": "2023-06-01",
    "x-api-key": "dummy",
    "Ocp-Apim-Subscription-Key": api_key,
    "user": os.environ.get("RUNNER_NAME", "gh-actions-runner"),
}

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

served_model = data.get("model", model)
print()
print(f"=== Knowledge Base Accuracy Check (model: {served_model}) ===")
print()

verdict = result.strip().split("\n", 1)[0].strip() if result.strip() else ""

if verdict == "PASS":
    print("PASS — no contradictions found")
    sys.exit(0)
elif verdict == "FAIL":
    print(result)
    print()
    print("Knowledge base has claims that contradict the code changes.")
    print("Update the relevant AGENTS.md or docs/knowledge/ files.")
    sys.exit(1)
else:
    print(f"Unexpected LLM verdict (first line: {verdict!r}) — treating as inconclusive")
    print(result[:500])
    sys.exit(2)
PYEOF
