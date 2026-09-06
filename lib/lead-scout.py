#!/usr/bin/env python3
"""Find people describing a problem PlausiDen solves, score them, draft a reply.

Discovery, triage and drafting run unattended. Posting is deliberately a
separate step, because each channel has its own rules about automated
submission and they are not interchangeable.

On not getting flagged: the defence here is legitimacy, not stealth.
Every request carries a descriptive User-Agent naming the tool and a
contact address, requests are rate-limited well under each source's
published ceiling, and nothing pretends to be a browser. Sources that
require an authenticated app get one rather than a workaround — Reddit
now serves HTML instead of JSON to unauthenticated datacenter IPs, which
is exactly the wall you are supposed to hit, and the answer is an OAuth
app, not a spoofed request.

  lead-scout.py [--source hn,lobsters,reddit] [--min-score 7] [--limit 20]
                [--notify] [--dry-run]
"""
import argparse, hashlib, json, os, subprocess, sys, time, urllib.parse, urllib.request

UA = ("PlausiDenLeadScout/0.1 (+https://plausiden.com; "
      "contact william@plausiden.com)")
STATE = os.environ.get("SCOUT_STATE", "/var/lib/lead-scout/seen")
CLAUDE = os.environ.get("CLAUDE_BIN", "/root/.local/bin/claude")
NOTIFY = os.environ.get("SCOUT_NOTIFY", "/home/paul/projects/claude-tools/lib/notify.sh")
TRIAGE_MODEL = os.environ.get("SCOUT_TRIAGE_MODEL", "claude-haiku-4-5-20251001")
DRAFT_MODEL = os.environ.get("SCOUT_DRAFT_MODEL", "claude-sonnet-5")

# What PlausiDen actually sells, phrased the way people complain about it.
QUERIES = [
    "custom software", "SaaS bloat", "too many subscriptions",
    "self-hosting", "migrate off", "vendor lock-in",
    "our IT guy", "managed service provider", "data privacy compliance",
]


def http_json(url, timeout=20):
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
    return json.loads(body)


def seen_load():
    try:
        with open(STATE) as f:
            return set(l.strip() for l in f if l.strip())
    except OSError:
        return set()


def seen_add(keys):
    os.makedirs(os.path.dirname(STATE), exist_ok=True)
    with open(STATE, "a") as f:
        for k in keys:
            f.write(k + "\n")


# ── sources ─────────────────────────────────────────────────────────────
def from_hn(limit):
    out = []
    for q in QUERIES:
        url = ("https://hn.algolia.com/api/v1/search_by_date?query="
               + urllib.parse.quote(f'"{q}"') + "&tags=comment&hitsPerPage=10")
        try:
            d = http_json(url)
        except Exception as e:
            print(f"  hn: {q}: {e}", file=sys.stderr); continue
        for h in d.get("hits", []):
            text = (h.get("comment_text") or "").strip()
            if len(text) < 120:          # one-liners are never a real lead
                continue
            out.append({
                "source": "hn", "id": "hn:" + str(h.get("objectID")),
                "author": h.get("author") or "?",
                "text": text[:2000], "matched": q,
                "url": f"https://news.ycombinator.com/item?id={h.get('objectID')}",
            })
        time.sleep(0.7)                   # well under Algolia's ceiling
    return out[:limit]


def from_lobsters(limit):
    try:
        d = http_json("https://lobste.rs/newest.json")
    except Exception as e:
        print(f"  lobsters: {e}", file=sys.stderr); return []
    out = []
    for s in d:
        text = (s.get("description_plain") or s.get("title") or "").strip()
        if len(text) < 120:
            continue
        out.append({
            "source": "lobsters", "id": "lob:" + str(s.get("short_id")),
            "author": (s.get("submitter_user") or {}).get("username")
                      if isinstance(s.get("submitter_user"), dict) else str(s.get("submitter_user")),
            "text": text[:2000], "matched": "newest",
            "url": s.get("comments_url") or s.get("url"),
        })
    return out[:limit]


def from_reddit(limit):
    """Requires a registered OAuth app.

    Reddit serves HTML rather than JSON to unauthenticated datacenter
    IPs. That is a deliberate access control, so the fix is credentials,
    not a disguised request. Create an app at
    https://www.reddit.com/prefs/apps (type: script) and put the values in
    /tank/secrets/reddit.env as REDDIT_CLIENT_ID / REDDIT_CLIENT_SECRET /
    REDDIT_USERNAME / REDDIT_PASSWORD.
    """
    env = "/tank/secrets/reddit.env"
    if not os.path.exists(env):
        print("  reddit: skipped — no /tank/secrets/reddit.env "
              "(create an OAuth app; unauthenticated access is blocked by Reddit, "
              "and working around that is not on the table)", file=sys.stderr)
        return []
    cfg = {}
    for line in open(env):
        if "=" in line and not line.strip().startswith("#"):
            k, v = line.strip().split("=", 1)
            cfg[k] = v.strip().strip('"')
    try:
        data = urllib.parse.urlencode({
            "grant_type": "password",
            "username": cfg["REDDIT_USERNAME"], "password": cfg["REDDIT_PASSWORD"],
        }).encode()
        auth = urllib.parse.quote(cfg["REDDIT_CLIENT_ID"]) + ":" + urllib.parse.quote(cfg["REDDIT_CLIENT_SECRET"])
        import base64
        req = urllib.request.Request(
            "https://www.reddit.com/api/v1/access_token", data=data,
            headers={"User-Agent": UA,
                     "Authorization": "Basic " + base64.b64encode(auth.encode()).decode()})
        tok = json.loads(urllib.request.urlopen(req, timeout=20).read())["access_token"]
    except Exception as e:
        print(f"  reddit: auth failed: {e}", file=sys.stderr); return []

    out = []
    for sub in ("smallbusiness", "msp", "sysadmin", "selfhosted"):
        try:
            req = urllib.request.Request(
                f"https://oauth.reddit.com/r/{sub}/new?limit=25",
                headers={"User-Agent": UA, "Authorization": "Bearer " + tok})
            d = json.loads(urllib.request.urlopen(req, timeout=20).read())
        except Exception as e:
            print(f"  reddit r/{sub}: {e}", file=sys.stderr); continue
        for c in d["data"]["children"]:
            p = c["data"]
            text = (p.get("selftext") or p.get("title") or "").strip()
            if len(text) < 120:
                continue
            out.append({
                "source": f"r/{sub}", "id": "rd:" + p["id"], "author": p.get("author"),
                "text": text[:2000], "matched": sub,
                "url": "https://reddit.com" + p.get("permalink", ""),
            })
        time.sleep(1.2)                   # Reddit asks for <= 1 req/sec
    return out[:limit]


SOURCES = {"hn": from_hn, "lobsters": from_lobsters, "reddit": from_reddit}


# ── the two-stage LLM pipeline ──────────────────────────────────────────
def claude(prompt, model, timeout=180):
    try:
        r = subprocess.run([CLAUDE, "-p", "--model", model], input=prompt,
                           capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip()
    except Exception as e:
        print(f"  claude: {e}", file=sys.stderr); return ""


TRIAGE = """Analyse this post. On a scale of 1-10, does the author have a
legitimate, unsolved technical or business problem related to software
engineering, custom app development, systems infrastructure, data privacy,
or tech-stack bloat — something a small consultancy could genuinely help with?

Score 1-3 if it is news, opinion, an announcement, or already solved.
Score 8+ ONLY if a specific person has a specific unsolved problem AND
would plausibly welcome an answer.

Return ONLY minified JSON: {"score": <int>, "reason": "<12 words max>"}

POST:
%s"""

DRAFT = """You are the lead systems architect at PlausiDen, a small firm doing
custom software, self-hosted infrastructure and privacy engineering.

Write a reply to the post below. Requirements:
- Diagnose the actual bottleneck and say what breaks underneath it.
- Give something concrete and useful even if they never hire anyone.
- No pitch, no "we offer", no link, no sign-off.
- Casual but authoritative, like an engineer who has fixed this before.
- Under 150 words. Specific beats comprehensive.
- If you cannot say something genuinely useful, reply exactly: SKIP

POST:
%s"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", default="hn,lobsters")
    ap.add_argument("--min-score", type=int, default=7)
    ap.add_argument("--limit", type=int, default=20)
    ap.add_argument("--notify", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    seen = seen_load()
    items, fresh = [], []
    for name in [s.strip() for s in a.source.split(",") if s.strip()]:
        fn = SOURCES.get(name)
        if not fn:
            print(f"unknown source: {name}", file=sys.stderr); continue
        got = fn(a.limit)
        print(f"  {name}: {len(got)} candidate(s)")
        items += got

    for it in items:
        if it["id"] not in seen:
            fresh.append(it); seen.add(it["id"])
    print(f"  {len(fresh)} new (rest already seen)")

    hits = []
    for it in fresh[: a.limit]:
        raw = claude(TRIAGE % it["text"], TRIAGE_MODEL, timeout=120)
        try:
            v = json.loads(raw[raw.find("{"): raw.rfind("}") + 1])
        except Exception:
            continue
        it["score"], it["reason"] = v.get("score", 0), v.get("reason", "")
        if it["score"] >= a.min_score:
            hits.append(it)

    print(f"  {len(hits)} scored >= {a.min_score}")
    for it in hits:
        it["draft"] = claude(DRAFT % it["text"], DRAFT_MODEL, timeout=240)

    hits = [h for h in hits if h.get("draft") and h["draft"].strip() != "SKIP"]

    for it in hits:
        print("\n" + "=" * 72)
        print(f"[{it['score']}/10] {it['source']} — {it['reason']}")
        print(f"  {it['url']}")
        print(f"  by {it['author']}: {it['text'][:160].strip()}...")
        print(f"\n  DRAFT:\n{it['draft']}")
        if a.notify and os.access(NOTIFY, os.X_OK):
            body = f"{it['url']}\n\n{it['text'][:220]}...\n\n--- draft ---\n{it['draft']}"
            subprocess.run([NOTIFY, "--key", "lead:" + it["id"],
                            "--title", f"Lead {it['score']}/10 — {it['source']}",
                            "--tags", "mag"], input=body, text=True)

    if not a.dry_run and fresh:
        seen_add([i["id"] for i in fresh])
    print(f"\n{len(hits)} lead(s) worth answering.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
