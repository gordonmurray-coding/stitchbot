#!/usr/bin/env python3
"""
Analyze a StitchBot metrics log (stitchbot_metrics.jsonl).

The question this answers: on a real Kaspa node, does *orphaned work* actually rise when tip width
exceeds the parent-merge cap? If it doesn't, the "narrow the DAG to save work" thesis is refuted and
the node-side mechanism isn't worth building. If it does, there's a measured efficiency gap.

Usage:  python3 analyze.py [path]     (default: stitchbot_metrics.jsonl)
Stdlib only — no plotting deps.
"""
import json, sys, math
from collections import Counter, defaultdict

PATH = sys.argv[1] if len(sys.argv) > 1 else "stitchbot_metrics.jsonl"


def load(path):
    rows = []
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    except FileNotFoundError:
        sys.exit(f"no metrics file at {path!r} — is the monitor running?")
    return rows


def pct(sorted_vals, p):
    if not sorted_vals:
        return 0
    k = (len(sorted_vals) - 1) * p
    lo = math.floor(k)
    hi = math.ceil(k)
    if lo == hi:
        return sorted_vals[int(k)]
    return sorted_vals[lo] * (hi - k) + sorted_vals[hi] * (k - lo)


def bar(n, mx, width=40):
    return "█" * int(round(width * n / mx)) if mx else ""


def hms(secs):
    secs = int(secs)
    h, m, s = secs // 3600, (secs % 3600) // 60, secs % 60
    return (f"{h}h" if h else "") + (f"{m}m" if (m or h) else "") + f"{s}s"


def mean(xs):
    return sum(xs) / len(xs) if xs else 0.0


def main():
    rows = load(PATH)
    if len(rows) < 5:
        sys.exit(f"only {len(rows)} records — let the monitor collect more, then re-run.")

    net = rows[-1].get("net", "?")
    span = (rows[-1]["t"] - rows[0]["t"]) / 1000.0
    tips = [r["tips"] for r in rows]
    cap = max((r.get("max_parents", 0) for r in rows), default=0)
    # Recompute excess post-hoc vs the global observed cap. The live tip_excess used a cap estimate
    # that grew as wider-parent blocks arrived, so early samples over-reported excess.
    excess = [max(0, t - cap) for t in tips]
    red = [r.get("red_rate", 0.0) * 100 for r in rows]  # percent
    tips_sorted = sorted(tips)

    W = 62
    print("═" * W)
    print(f" StitchBot DAG-health analysis · {net} · {PATH}")
    print(f" {len(rows)} samples over {hms(span)}  ({span/max(1,len(rows)):.1f}s/sample)")
    print("═" * W)

    # --- tip width distribution ---
    print("\nTIP WIDTH DISTRIBUTION")
    hist = Counter(tips)
    mx = max(hist.values())
    for w in range(min(hist), max(hist) + 1):
        c = hist.get(w, 0)
        flag = "  ← over merge cap" if w > cap else ""
        print(f"  {w:>3} tips │ {bar(c, mx, 34):<34} {c:>6}{flag}")
    print(f"  mean {mean(tips):.2f} · median {pct(tips_sorted,0.5):.0f} · "
          f"p95 {pct(tips_sorted,0.95):.0f} · max {max(tips)} (polled)")
    print(f"  widest single-block merge: {cap} parents → the DAG reached ≥{cap} tips at least once")
    print(f"  (a k-parent block proves ≥k simultaneous tips — parent counts catch spikes the 1s poll misses)")

    over = [i for i, e in enumerate(excess) if e > 0]
    print(f"  samples with tips OVER the cap: {len(over)} ({100*len(over)/len(rows):.1f}%)"
          f"  ·  max excess seen: {max(excess)}")

    # --- THE KEY TEST: orphan rate vs tip excess ---
    print("\nTHE TEST — orphan (red) rate grouped by tip excess")
    print("  excess = tips beyond the merge cap (the only regime that can waste work)")
    buckets = [("0 (within cap)", lambda e: e == 0),
               ("1–3 over",       lambda e: 1 <= e <= 3),
               ("4–6 over",       lambda e: 4 <= e <= 6),
               ("7+ over",        lambda e: e >= 7)]
    print(f"  {'bucket':<16}{'samples':>9}{'mean orphan':>14}{'max orphan':>13}")
    for name, f in buckets:
        rr = [red[i] for i, e in enumerate(excess) if f(e)]
        if rr:
            print(f"  {name:<16}{len(rr):>9}{mean(rr):>13.3f}%{max(rr):>12.3f}%")
        else:
            print(f"  {name:<16}{0:>9}{'—':>14}{'—':>13}")

    # orphan by tip-width band too
    print("\n  orphan rate by tip width:")
    bands = [(0, cap), (cap + 1, cap + 4), (cap + 5, 999)]
    for lo, hi in bands:
        rr = [red[i] for i, t in enumerate(tips) if lo <= t <= hi]
        label = f"{lo}–{hi if hi<999 else '∞'} tips"
        if rr:
            print(f"    {label:<14} mean {mean(rr):>7.3f}%  max {max(rr):>7.3f}%  (n={len(rr)})")

    # correlation
    if len(set(excess)) > 1:
        me, mr = mean(excess), mean(red)
        cov = sum((e - me) * (r - mr) for e, r in zip(excess, red))
        de = math.sqrt(sum((e - me) ** 2 for e in excess))
        dr = math.sqrt(sum((r - mr) ** 2 for r in red))
        corr = cov / (de * dr) if de and dr else 0.0
        print(f"\n  correlation(tip_excess, orphan_rate) = {corr:+.3f}")
    else:
        corr = None

    # --- fracture time ---
    fr = [r for r in rows if r.get("fracture")]
    episodes = sum(1 for i in range(1, len(rows))
                   if rows[i].get("fracture") and not rows[i-1].get("fracture"))
    maxdur = max((r.get("fracture_secs", 0) for r in rows), default=0)
    print(f"\nFRACTURE STATE (tips ≥ threshold or blue-spread large)")
    print(f"  fractured {100*len(fr)/len(rows):.1f}% of samples · {episodes} episodes · "
          f"longest {hms(maxdur)}")

    # --- verdict ---
    print("\n" + "═" * W)
    over_red = [red[i] for i, e in enumerate(excess) if e > 0]
    if not over:
        print(" VERDICT: no tips-over-cap event captured yet in this window.")
        print(f"          Widest merge seen: {cap} parents (≥{cap} tips), fully merged with")
        print(f"          {max(red):.3f}% orphan. To test the over-cap regime we need a spike with")
        print(f"          tips > {cap} (you've observed ~20). Keep collecting.")
    elif mean(over_red) < 0.5 and max(over_red) < 2.0:
        print(f" VERDICT: REFUTED so far. Across {len(over)} over-cap samples (excess up to")
        print(f"          {max(excess)}), orphan rate averaged {mean(over_red):.3f}% (max {max(over_red):.3f}%).")
        print("          Tips beyond the cap are re-merged within a block or two — GHOSTDAG")
        print("          absorbs the width. No measurable wasted work → the node-side")
        print("          mechanism is not justified by this data.")
    elif corr and corr > 0.3 and mean(over_red) > 2.0:
        print(f" VERDICT: SIGNAL. Orphan rate rises with tip excess (corr {corr:+.2f}); over-cap")
        print(f"          samples average {mean(over_red):.2f}% wasted work (max {max(over_red):.2f}%).")
        print("          A real efficiency gap — the node-side parent-selection experiment")
        print("          is now justified by evidence.")
    else:
        print(f" VERDICT: INCONCLUSIVE. Some over-cap samples ({len(over)}), orphan mean")
        print(f"          {mean(over_red):.2f}%. Collect more data / bigger spikes to separate")
        print("          signal from noise.")
    print("═" * W)


if __name__ == "__main__":
    main()
