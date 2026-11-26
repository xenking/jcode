#!/usr/bin/env python3
"""Re-fit the per-family token estimator weights from recorded sessions.

Walks ~/.jcode/sessions (or --sessions), pairs each assistant turn's provider-reported
prompt tokens with the content added since the previous turn, fits the class weights of
`crates/jcode-message-types/src/token_estimate.rs` per model family, and prints the
weights in milli-tokens plus the held-out error (70/30 split by session id).

Only counts and lengths are computed; no message text is printed or written.
Needs numpy and scipy (`python3 -m venv v && v/bin/pip install numpy scipy`).

    scripts/fit_token_estimator.py            # all families
    scripts/fit_token_estimator.py --family anthropic
"""
import argparse
import glob
import hashlib
import json
import os
import re
import sys

TEXT = ["letters", "words", "digits", "space", "space_runs", "punct", "cyrillic", "cyrillic_words", "cjk", "other"]
EXTRA = ["reasoning_bytes", "signature_bytes", "encrypted_bytes", "native_bytes", "text_blocks", "tool_calls"]
FEATURES = TEXT + EXTRA
# Priors, not fitted: too few recorded pairs contain CJK.
CJK_PRIOR = {"anthropic": 1.5, "openai": 1.1, "deepseek": 0.9}


def classify(text):
    """Class counts; must match `Estimator::scan` in token_estimate.rs."""
    counts = dict.fromkeys(TEXT, 0)
    run = None
    for ch in text:
        o = ord(ch)
        if o < 128:
            if ch.isalpha():
                counts["letters"] += 1
                counts["words"] += run != "letter"
                run = "letter"
            elif ch.isdigit():
                counts["digits"] += 1
                run = "digit"
            elif ch.isspace():
                counts["space"] += 1
                counts["space_runs"] += run != "space"
                run = "space"
            else:
                counts["punct"] += 1
                run = "punct"
        elif 0x400 <= o <= 0x4FF:
            counts["cyrillic"] += 1
            counts["cyrillic_words"] += run != "cyrillic"
            run = "cyrillic"
        elif o in range(0x3000, 0x3100) or o in range(0x3400, 0x4DC0) or o in range(0x4E00, 0xA000) \
                or o in range(0xAC00, 0xD7B0) or o in range(0xF900, 0xFB00) or o in range(0xFF00, 0xFFF0):
            counts["cjk"] += 1
            run = "cjk"
        elif ch.isspace():
            counts["space"] += 1
            counts["space_runs"] += run != "space"
            run = "space"
        else:
            counts["other"] += 1
            run = "other"
    return counts


def blen(value):
    return len(value.encode()) if isinstance(value, str) else 0


def message_features(message):
    feats = dict.fromkeys(FEATURES, 0)
    images = 0
    content = message["content"]
    if isinstance(content, str):
        content = [{"type": "text", "text": content}]

    def add_text(text):
        for key, value in classify(text).items():
            feats[key] += value

    for block in content:
        kind = block["type"]
        if kind == "text":
            if block["text"]:
                feats["text_blocks"] += 1
                add_text(block["text"])
        elif kind == "tool_result":
            add_text(block["content"])
        elif kind == "tool_use":
            feats["tool_calls"] += 1
            add_text(block["name"] + json.dumps(block["input"], ensure_ascii=False, separators=(",", ":")))
        elif kind in ("reasoning", "reasoning_trace"):
            feats["reasoning_bytes"] += blen(block["text"])
        elif kind == "anthropic_thinking":
            feats["reasoning_bytes"] += blen(block["thinking"])
            feats["signature_bytes"] += blen(block["signature"])
        elif kind == "open_a_i_reasoning":
            feats["encrypted_bytes"] += blen(block.get("encrypted_content") or "")
            feats["reasoning_bytes"] += sum(blen(part) for part in block.get("summary", []))
        elif kind == "provider_native":
            feats["native_bytes"] += blen(json.dumps(block["item"], separators=(",", ":")))
        elif kind == "image":
            images += 1
    return feats, images


def family_of(model):
    model = (model or "").lower()
    if "claude" in model:
        return "anthropic"
    if model.startswith("gpt") or re.match(r"o\d", model) or "codex" in model:
        return "openai"
    if "deepseek" in model:
        return "deepseek"
    return None


def total_input(usage):
    if usage.get("prompt_tokens") is not None:
        return usage["prompt_tokens"]
    return sum(usage.get(k) or 0 for k in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))


def load_session(directory, sid):
    with open(f"{directory}/{sid}.json") as fh:
        data = json.load(fh)
    messages, seen, models = list(data["messages"]), {m["id"] for m in data["messages"]}, {data.get("model")}
    journal = f"{directory}/{sid}.journal.jsonl"
    if os.path.exists(journal):
        with open(journal) as fh:
            for line in fh:
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                models.add((entry.get("meta") or {}).get("model"))
                for message in entry.get("append_messages") or []:
                    if message["id"] not in seen:
                        seen.add(message["id"])
                        messages.append(message)
    compactions = [e["timestamp"] for e in data.get("replay_events", []) if e.get("event") == "compaction"]
    return messages, models - {None, "unknown"}, compactions


def collect(directory):
    pairs = {}
    for path in sorted(glob.glob(f"{directory}/*.json")):
        sid = os.path.basename(path)[:-5]
        try:
            messages, models, compactions = load_session(directory, sid)
        except (OSError, ValueError, KeyError):
            continue
        family = family_of(next(iter(models))) if len(models) == 1 else None
        if family is None:
            continue
        digest = int(hashlib.sha1(sid.encode()).hexdigest()[:8], 16)
        feats = [message_features(m) for m in messages]
        turns = [i for i, m in enumerate(messages)
                 if m["role"] == "assistant" and m.get("token_usage") and total_input(m["token_usage"]) > 0]
        for previous, current in zip(turns, turns[1:]):
            if any(messages[previous].get("timestamp", "") <= t <= messages[current].get("timestamp", "") for t in compactions):
                continue
            delta = total_input(messages[current]["token_usage"]) - total_input(messages[previous]["token_usage"])
            if not 100 <= delta < 150_000:
                continue
            row, images = dict.fromkeys(FEATURES, 0), 0
            for k in range(previous, current):
                for key, value in feats[k][0].items():
                    row[key] += value
                images += feats[k][1]
            if images:  # images are costed per image, not fitted
                continue
            pairs.setdefault(family, []).append((digest, sid, row, images, delta))
    return pairs


def fit(pairs, family, np, nnls):
    x = np.array([[r[2][f] for f in FEATURES] for r in pairs])
    y = np.array([float(r[4]) for r in pairs])
    cjk = FEATURES.index("cjk")
    fixed = CJK_PRIOR.get(family, 1.2)
    cols = [i for i in range(len(FEATURES)) if i != cjk]
    y = y - x[:, cjk] * fixed
    weight = 1.0 / np.maximum(y, 1)
    for _ in range(10):
        beta, _ = nnls(x[:, cols] * weight[:, None], y * weight, maxiter=50_000)
        rel = np.abs(x[:, cols] @ beta - y) / np.maximum(y, 1)
        weight = 1.0 / np.maximum(y, 1) / np.sqrt(np.maximum(rel, 0.05))
    out = np.zeros(len(FEATURES))
    out[cols] = beta
    out[cjk] = fixed
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sessions", default=os.path.expanduser("~/.jcode/sessions"))
    parser.add_argument("--family", choices=sorted(CJK_PRIOR))
    args = parser.parse_args()
    try:
        import numpy as np
        from scipy.optimize import nnls
    except ImportError:
        sys.exit("needs numpy and scipy")
    for family, pairs in sorted(collect(args.sessions).items()):
        if args.family and family != args.family:
            continue
        train = [p for p in pairs if p[0] % 10 < 7]
        held = [p for p in pairs if p[0] % 10 >= 7]
        if len(train) < 200 or len(held) < 50:
            print(f"{family}: too little data ({len(train)} fit / {len(held)} held-out pairs), keep the generic table")
            continue
        beta = fit(train, family, np, nnls)
        x = np.array([[r[2][f] for f in FEATURES] for r in held])
        y = np.array([float(r[4]) for r in held])
        est = x @ beta
        err = np.abs(est - y) / y * 100
        print(f"{family}: {len(train)} fit / {len(held)} held-out pairs, "
              f"held-out error median {np.median(err):.1f}% p95 {np.percentile(err, 95):.1f}%")
        print("  milli-tokens: " + ", ".join(f"{f}={round(b * 1000)}" for f, b in zip(FEATURES, beta)))


if __name__ == "__main__":
    main()
