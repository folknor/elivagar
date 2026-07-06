#!/usr/bin/env python3
"""Digest a Claude Code session transcript (.jsonl).

Prints user messages in full and assistant text messages (truncated),
skipping tool calls/results, so a long session can be skimmed.

Usage: session_digest.py <transcript.jsonl> [max_assistant_chars]
"""
import json
import sys


def main() -> None:
    path = sys.argv[1]
    max_chars = int(sys.argv[2]) if len(sys.argv) > 2 else 600
    with open(path, encoding="utf-8") as f:
        for line in f:
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            typ = rec.get("type")
            if typ not in ("user", "assistant"):
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
            if "<local-command-stdout>" in text or "<command-name>" in text:
                continue
            ts = (rec.get("timestamp") or "")[:16]
            if typ == "user":
                if "tool_result" in str(content)[:200]:
                    continue
                print(f"\n===== USER [{ts}] =====")
                print(text)
            else:
                print(f"\n--- assistant [{ts}] ---")
                if len(text) > max_chars:
                    print(text[:max_chars] + f" …[+{len(text) - max_chars} chars]")
                else:
                    print(text)


if __name__ == "__main__":
    main()
