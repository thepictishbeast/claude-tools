#!/usr/bin/env python3
"""What the first-party analytics actually know, minus the bot noise.

The analytics store holds 86k hits across 17 sites with path, status and
referrer. Read naively it says the most-wanted page on the estate is
/wp-admin/install.php, because 859 scanners asked for it. Every raw
"top 404" list on this data is a list of WordPress probes.

So the filtering is the product. What survives is genuinely useful:

  * assets real browsers request and do not get — a missing favicon is
    404ing on every visit and shows as a blank square in a bookmark bar
  * pages people reach from a search engine, which is what they were
    actually looking for
  * which of the sites send traffic to each other
  * what holds attention once they arrive

It also emits topic candidates for content-forge, so posts get written
about what visitors are looking for rather than what we assume.

  market-intel.py [--days 30] [--queue] [--site HOST]
"""
import argparse, os, re, sqlite3, sys

DB = os.environ.get("ANALYTICS_DB", "/var/lib/plausiden-analytics/analytics.db")
QUEUE = os.environ.get("CONTENT_QUEUE", "/var/lib/content-forge/queue.txt")

# Vulnerability scanning, not demand. Every one of these was verified
# present in the live data before being listed; the point is to remove
# noise, not to guess at it.
BOT = re.compile(
    r"(^/wp[-/]|wordpress|xmlrpc|/\.env|/\.git|phpmyadmin|/vendor/|/cgi-bin/"
    r"|\.php$|/admin(istrator)?/|/autodiscover/|/owa/|/boaform|/shell|/setup"
    r"|/telescope|/actuator|/config\.json|/backup|/\.well-known/traffic-advice)",
    re.I)

# Assets a real browser asks for on its own. A 404 here is a defect the
# visitor never reports, because it does not stop them reading.
BROWSER_ASSET = re.compile(r"^/(favicon\.ico|robots\.txt|apple-touch-icon.*|sitemap\.xml|manifest\.json)$", re.I)

SEARCH = re.compile(r"(google|bing|duckduckgo|ecosia|yandex|brave|startpage|search\.)", re.I)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=30)
    ap.add_argument("--site")
    ap.add_argument("--queue", action="store_true",
                    help="append topic candidates to the content-forge queue")
    a = ap.parse_args()

    if not os.path.exists(DB):
        print(f"market-intel: no analytics db at {DB}", file=sys.stderr); return 2
    try:
        db = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    except sqlite3.Error as e:
        print(f"market-intel: {e}", file=sys.stderr); return 2

    since = f"strftime('%s','now','-{a.days} days')"
    where = f"h.ts > {since}" + (" and s.host = :host" if a.site else "")
    args = {"host": a.site} if a.site else {}

    rows = db.execute(
        f"select s.host, h.path, h.status, h.referer_host, h.visitor_hash "
        f"from hit h join site s on s.id = h.site_id where {where}", args).fetchall()
    if not rows:
        print(f"market-intel: no hits in the last {a.days} days"); return 0

    human = [r for r in rows if not BOT.search(r[1] or "")]
    print(f"{len(rows)} requests in {a.days} days — {len(rows)-len(human)} were scanners, "
          f"{len(human)} left")

    # 1. Broken assets: the defect nobody reports.
    print("\nAssets a browser asked for and did not get")
    broken = {}
    for host, path, status, _, _ in human:
        if status == 404 and BROWSER_ASSET.match(path or ""):
            broken[(host, path)] = broken.get((host, path), 0) + 1
    if broken:
        for (host, path), n in sorted(broken.items(), key=lambda kv: -kv[1])[:12]:
            print(f"  {n:>5}  {host}{path}")
    else:
        print("  none — every site serves its favicon, robots.txt and sitemap")

    # 2. Pages people wanted that do not exist (real ones, not probes).
    print("\nPages requested that returned 404 (scanners already removed)")
    gaps = {}
    for host, path, status, _, _ in human:
        if status == 404 and not BROWSER_ASSET.match(path or ""):
            gaps[(host, path)] = gaps.get((host, path), 0) + 1
    shown = [kv for kv in sorted(gaps.items(), key=lambda kv: -kv[1]) if kv[1] > 1][:10]
    if shown:
        for (host, path), n in shown:
            print(f"  {n:>5}  {host}{path}")
    else:
        print("  none worth noting")

    # 3. Search arrivals — what they were actually looking for.
    print("\nArrived from a search engine")
    land = {}
    for host, path, status, ref, _ in human:
        if ref and SEARCH.search(ref) and status < 400:
            land[(host, path)] = land.get((host, path), 0) + 1
    if land:
        for (host, path), n in sorted(land.items(), key=lambda kv: -kv[1])[:10]:
            print(f"  {n:>5}  {host}{path}")
    else:
        print("  none — nothing is arriving from search")

    # 4. Which sites feed each other.
    print("\nReferred from another site")
    refs = {}
    for host, path, status, ref, _ in human:
        if ref and not SEARCH.search(ref):
            refs[ref] = refs.get(ref, 0) + 1
    for ref, n in sorted(refs.items(), key=lambda kv: -kv[1])[:10]:
        print(f"  {n:>5}  {ref}")

    # 5. What actually holds attention, by unique visitor rather than hit
    #    count, so one person refreshing does not look like an audience.
    print("\nMost-read pages, counted by distinct visitor")
    seen = {}
    for host, path, status, _, vh in human:
        if status < 400 and vh and not path.startswith("/static"):
            seen.setdefault((host, path), set()).add(vh)
    for (host, path), v in sorted(seen.items(), key=lambda kv: -len(kv[1]))[:12]:
        print(f"  {len(v):>5}  {host}{path}")

    if a.queue:
        # Only search landings become topics. A 404 is a routing bug, not a
        # question, and writing a post about one would be answering nobody.
        cands = []
        for (host, path), n in sorted(land.items(), key=lambda kv: -kv[1])[:5]:
            slug = path.strip("/").replace("-", " ").replace("/", " ")
            if slug:
                cands.append(f"What someone searching for \"{slug}\" actually needs to know")
        if cands:
            os.makedirs(os.path.dirname(QUEUE), exist_ok=True)
            existing = set()
            if os.path.exists(QUEUE):
                existing = {l.strip() for l in open(QUEUE)}
            new = [c for c in cands if c not in existing]
            with open(QUEUE, "a") as f:
                for c in new:
                    f.write(c + "\n")
            print(f"\nqueued {len(new)} new topic(s) -> {QUEUE}")
        else:
            print("\nno search landings yet, so no topics queued — "
                  "topics from guesswork are what this is meant to replace")
    return 0


if __name__ == "__main__":
    sys.exit(main())
