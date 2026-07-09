#!/usr/bin/env python3
"""Print the first N assistant-written text messages from a Claude Code
session transcript (JSONL). Tool calls and tool results are skipped; only
the text blocks the agent actually wrote are shown.

Usage: extract_session_messages.py <transcript.jsonl> [count]
"""
import json
import sys


def main() -> None:
    path = sys.argv[1]
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    found = 0
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("type") != "assistant":
                continue
            msg = rec.get("message") or {}
            content = msg.get("content")
            texts = []
            if isinstance(content, str):
                texts.append(content)
            elif isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "text":
                        texts.append(block.get("text", ""))
            text = "\n".join(t for t in texts if t.strip())
            if not text.strip():
                continue
            found += 1
            ts = rec.get("timestamp", "?")
            print(f"===== message {found} ({ts}) =====")
            print(text)
            print()
            if found >= count:
                break
    if found < count:
        print(f"(only {found} assistant text messages found)")


if __name__ == "__main__":
    main()
