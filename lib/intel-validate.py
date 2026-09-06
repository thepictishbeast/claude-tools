#!/usr/bin/env python3
"""Reject intel events that would poison the analysis.

Two Claude sessions feed this store. A contract nobody can enforce is a
wish list, so this is the enforcement: run it before appending, and a bad
event never reaches the file rather than being discovered weeks later
inside a conclusion.

What it refuses, and why each one matters more than it looks:

  * an unsourced claim. The failure mode of intelligence work is not
    missing data, it is confident data with nothing behind it. "MSPs are
    raising prices" is worthless; the same sentence with a URL is a
    finding. `evidence` is mandatory and must be a URL or a quotation.

  * `basis: observed` on something nobody observed. Inference is fine and
    often necessary — it just has to be labelled, or a guess becomes a
    fact the moment it is aggregated with real ones.

  * personal data. We sell confidentiality. An intel store full of
    strangers' email addresses would be indefensible in the first
    conversation where it came up, and the permalink identifies the
    source perfectly well without it.

  * a future timestamp, or one from before this file existed. Both mean
    the clock or the parser is wrong, and a wrong clock silently ruins
    every trend the store is meant to produce.

  intel-validate.py --file batch.jsonl      exit 0 = every line acceptable
  intel-validate.py --stdin
"""
import argparse, datetime, json, re, sys

KINDS = {"lead", "engagement", "competitor", "market_signal",
         "outreach", "content", "service_demand"}
BASES = {"observed", "reported", "inferred"}
REQUIRED = ("ts", "kind", "source", "observed_by", "basis", "confidence", "evidence", "data")

EMAIL = re.compile(r"[\w.+-]+@[\w-]+\.[\w.]+")
# Deliberately narrow. A loose digit-run pattern matches the ISO
# timestamp on every single event, and a validator that flags healthy
# input gets ignored within a day — the same way a monitor that is always
# red stops being read. Require punctuation a phone number actually has.
PHONE = re.compile(r"(?<![\d-])(?:\+\d{1,3}[ -]?)?(?:\(\d{3}\)[ -]?|\d{3}[ -])\d{3}[ -]\d{4}(?![\d-])")
IPV4 = re.compile(r"(?<!\d)(\d{1,3}\.){3}\d{1,3}(?!\d)")
URL = re.compile(r"https?://\S+")

# Nothing predates the contract. An event stamped earlier is a parsing
# bug, not history.
EPOCH = datetime.datetime(2026, 9, 1, tzinfo=datetime.timezone.utc)


def check(ev, idx):
    out = []
    if not isinstance(ev, dict):
        return [f"line {idx}: not a JSON object"]

    for f in REQUIRED:
        if f not in ev:
            out.append(f"line {idx}: missing required field '{f}'")
    if out:
        return out

    if ev["kind"] not in KINDS:
        out.append(f"line {idx}: kind '{ev['kind']}' is not one of {sorted(KINDS)}")
    if ev["basis"] not in BASES:
        out.append(f"line {idx}: basis '{ev['basis']}' is not one of {sorted(BASES)}")

    try:
        ts = datetime.datetime.fromisoformat(str(ev["ts"]).replace("Z", "+00:00"))
        if ts.tzinfo is None:
            out.append(f"line {idx}: ts has no timezone — use UTC with a trailing Z")
        else:
            now = datetime.datetime.now(datetime.timezone.utc)
            if ts > now + datetime.timedelta(minutes=5):
                out.append(f"line {idx}: ts is in the future ({ev['ts']}) — check the clock")
            if ts < EPOCH:
                out.append(f"line {idx}: ts predates the contract ({ev['ts']}) — likely a parse error")
    except (ValueError, TypeError):
        out.append(f"line {idx}: ts '{ev['ts']}' is not ISO 8601")

    try:
        c = float(ev["confidence"])
        if not 0.0 <= c <= 1.0:
            out.append(f"line {idx}: confidence {c} is outside 0.0-1.0")
    except (ValueError, TypeError):
        out.append(f"line {idx}: confidence is not a number")

    evid = str(ev.get("evidence") or "").strip()
    if len(evid) < 12:
        out.append(f"line {idx}: evidence is empty or too short to check — "
                   "give a URL or a verbatim quote")
    elif ev.get("basis") == "observed" and not URL.search(evid) and '"' not in evid:
        out.append(f"line {idx}: basis is 'observed' but evidence is neither a URL nor a "
                   "quotation — mark it 'inferred' or cite something")

    if not isinstance(ev.get("data"), dict) or not ev["data"]:
        out.append(f"line {idx}: data must be a non-empty object")

    # Personal data, anywhere in the record EXCEPT the timestamp and any
    # URL. Both are structurally full of digits and neither is personal
    # data, so scanning them only produces false positives.
    scan = {k: v for k, v in ev.items() if k != "ts"}
    blob = URL.sub(" ", json.dumps(scan))
    if EMAIL.search(blob):
        out.append(f"line {idx}: contains what looks like an email address — "
                   "use the public handle or permalink instead")
    if IPV4.search(blob):
        out.append(f"line {idx}: contains what looks like an IP address — "
                   "store a hash if you need to distinguish visitors")
    m = PHONE.search(blob)
    if m and not URL.search(m.group(0)):
        out.append(f"line {idx}: contains what looks like a phone number")

    if ev["kind"] == "market_signal":
        n = ev["data"].get("sample_size")
        if not isinstance(n, int) or n < 1:
            out.append(f"line {idx}: market_signal needs an integer sample_size >= 1 — "
                       "a trend resting on nothing is an opinion")
    if ev["kind"] == "lead" and not ev["data"].get("permalink"):
        out.append(f"line {idx}: lead needs data.permalink so the source can be re-found")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file")
    ap.add_argument("--stdin", action="store_true")
    a = ap.parse_args()
    if a.file:
        lines = open(a.file, encoding="utf-8").read().splitlines()
    elif a.stdin:
        lines = sys.stdin.read().splitlines()
    else:
        print("intel-validate: --file or --stdin", file=sys.stderr); return 2

    problems, n = [], 0
    for i, line in enumerate(lines, 1):
        if not line.strip():
            continue
        n += 1
        if len(line.encode()) >= 4096:
            problems.append(f"line {i}: over 4096 bytes — appends are only atomic below that, "
                            "and two writers will interleave")
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError as e:
            problems.append(f"line {i}: not valid JSON ({e.msg})"); continue
        problems += check(ev, i)

    if problems:
        print(f"{len(problems)} problem(s) across {n} event(s):", file=sys.stderr)
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        return 1
    print(f"intel-validate: {n} event(s) OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
