"""
Empirical test of Claude API prompt-cache TTL (5-minute default).

Runtime: ~7 minutes. Requires ANTHROPIC_API_KEY in env.

  pip install anthropic
  python cache_ttl_test.py

Phases:
  1. Cold call    -> expect cache_creation_input_tokens > 0
  2. Warm call    -> expect cache_read_input_tokens     > 0   (immediate hit)
  3. Sleep 5m40s without touching the prefix
  4. Expired call -> expect cache_creation_input_tokens > 0   (TTL elapsed -> rewrite)

The 5-minute TTL is a *sliding* window — every hit refreshes it. The wait
between phase 2 and phase 4 must therefore exceed 5 min with no intervening
requests against the same prefix.
"""

import os
import sys
import time
from anthropic import Anthropic

MODEL = "claude-haiku-4-5"   # 4096-token min cacheable prefix
WAIT_SECONDS = 340           # 5 min 40 s — safely past the 5-min TTL

# Build a stable cacheable prefix > 4096 tokens (~4 chars/token => ~17 KB).
# Same bytes every run, so the cache key is identical across phases.
PASSAGE = (
    "The Claude API supports prompt caching to reduce cost and latency for "
    "repeated context. Cache breakpoints are declared with cache_control on "
    "the trailing block of the prefix you want cached. The cache key is the "
    "exact byte sequence of tools, system, and messages rendered in order. "
    "Any change — a timestamp, a reordered tool, a single whitespace edit — "
    "invalidates everything from that point onward. Verification is done via "
    "the usage object on each response: cache_creation_input_tokens counts "
    "tokens written to the cache, cache_read_input_tokens counts tokens "
    "served from the cache, and input_tokens counts uncached tokens billed "
    "at full price. The default TTL is five minutes, refreshed on every hit. "
)
SYSTEM_PROMPT = [
    {
        "type": "text",
        "text": PASSAGE * 80,   # ~17 KB, comfortably > 4096 tokens
        "cache_control": {"type": "ephemeral"},
    }
]

client = Anthropic()


def call(label: str) -> dict:
    resp = client.messages.create(
        model=MODEL,
        max_tokens=40,
        system=SYSTEM_PROMPT,
        messages=[{"role": "user", "content": "Reply with the single word: ok"}],
    )
    u = resp.usage
    creation = getattr(u, "cache_creation_input_tokens", 0) or 0
    read = getattr(u, "cache_read_input_tokens", 0) or 0
    print(
        f"[{label:7}] input={u.input_tokens:<5} "
        f"cache_creation={creation:<5} cache_read={read:<5} "
        f"output={u.output_tokens}"
    )
    return {"creation": creation, "read": read}


def expect(label: str, cond: bool, msg: str) -> None:
    mark = "PASS" if cond else "FAIL"
    print(f"  {mark}: {msg}")
    if not cond:
        sys.exit(1)


def main() -> None:
    if not os.environ.get("ANTHROPIC_API_KEY"):
        sys.exit("ANTHROPIC_API_KEY not set")

    print(f"Model: {MODEL}\n")

    print("Phase 1: cold call (expect cache write)")
    r1 = call("cold")
    expect("phase1", r1["creation"] > 0, "cache_creation_input_tokens > 0")
    expect("phase1", r1["read"] == 0,    "cache_read_input_tokens == 0")

    print("\nPhase 2: warm call immediately (expect cache hit)")
    r2 = call("warm")
    expect("phase2", r2["read"] > 0,     "cache_read_input_tokens > 0")
    expect("phase2", r2["creation"] == 0, "cache_creation_input_tokens == 0")

    print(f"\nPhase 3: sleep {WAIT_SECONDS}s (> 5 min TTL, no requests)…")
    for remaining in range(WAIT_SECONDS, 0, -30):
        print(f"  {remaining}s left…")
        time.sleep(min(30, remaining))

    print("\nPhase 4: post-TTL call (expect cache rewrite, NOT a hit)")
    r3 = call("expired")
    expect("phase4", r3["creation"] > 0, "cache_creation_input_tokens > 0 (TTL expired)")
    expect("phase4", r3["read"] == 0,    "cache_read_input_tokens == 0")

    print("\nAll phases passed: 5-minute TTL behavior confirmed.")


if __name__ == "__main__":
    main()
