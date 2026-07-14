#!/usr/bin/env python3
"""site-stats: aggregate, privacy-respecting traffic digest from Caddy logs.

Parses Caddy console-format access logs (timestamp<TAB>...<TAB>{json}) and
emits a per-site digest: pageviews, approximate unique visitors, top pages,
referrers, status breakdown, and a rough human/bot split. Aggregate-only by
design: distinct IPs are COUNTED for the window and immediately discarded -
nothing per-visitor is stored or emitted. Matches the sites' privacy-policy
promise ("aggregate, non-identifying traffic statistics").

Usage:
  site-stats.py [--hours 24] [--mail william@plausiden.com] [LOG ...]
Defaults to every /var/log/caddy/*.log.
"""
import argparse
import datetime as dt
import glob
import json
import re
import subprocess
import sys
import urllib.parse
from collections import Counter, defaultdict

BOT_RE = re.compile(
    r"bot|crawl|spider|slurp|curl|wget|python-requests|httpx|monitor|probe|scan|headless",
    re.I,
)
ASSET_RE = re.compile(r"^/(assets|css|js)/|\.(png|jpe?g|webp|gif|svg|ico|css|js|xml|txt|woff2?)(\?|$)", re.I)
LINE_RE = re.compile(r"^(\d{4}/\d{2}/\d{2} \d{2}:\d{2}:\d{2})\.\d+\t.*?(\{.*\})\s*$")


def parse_args():
    p = argparse.ArgumentParser()
    p.add_argument("--hours", type=int, default=24)
    p.add_argument("--mail", metavar="ADDR")
    p.add_argument("logs", nargs="*", default=None)
    return p.parse_args()


def digest(paths, hours):
    cutoff = dt.datetime.now() - dt.timedelta(hours=hours)
    sites = defaultdict(lambda: {
        "pageviews": 0, "requests": 0, "ips": set(), "pages": Counter(),
        "referrers": Counter(), "status": Counter(), "bots": 0,
        # Consent-gated beacon pings (GET /t): aggregated by their labeled
        # dimensions, kept OUT of the pageview numbers above.
        "pings": 0, "ping_pages": Counter(), "ping_vw": Counter(),
        "ping_theme": Counter(), "ping_refs": Counter(),
    })
    for path in paths:
        try:
            fh = open(path, "r", errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                m = LINE_RE.match(line)
                if not m:
                    continue
                try:
                    ts = dt.datetime.strptime(m.group(1), "%Y/%m/%d %H:%M:%S")
                except ValueError:
                    continue
                if ts < cutoff:
                    continue
                try:
                    rec = json.loads(m.group(2))
                except json.JSONDecodeError:
                    continue
                req = rec.get("request", {})
                host = req.get("host", "unknown")
                s = sites[host]
                s["requests"] += 1
                status = rec.get("status", 0)
                s["status"][f"{status // 100}xx"] += 1
                ua = (req.get("headers", {}).get("User-Agent") or [""])[0]
                if BOT_RE.search(ua):
                    s["bots"] += 1
                    continue
                full_uri = req.get("uri", "/")
                uri = full_uri.split("?")[0]
                if uri == "/t":
                    # Opt-in usage ping (mom-site beacon.js, consent-gated).
                    # Count its labeled dimensions only; never a pageview,
                    # never tied to an IP.
                    if 200 <= status < 300:
                        q = urllib.parse.parse_qs(urllib.parse.urlparse(full_uri).query)
                        s["pings"] += 1
                        for param, counter in (("p", "ping_pages"), ("vw", "ping_vw"),
                                               ("th", "ping_theme"), ("r", "ping_refs")):
                            v = q.get(param, [""])[0]
                            if v:
                                s[counter][v[:80]] += 1
                    continue
                if ASSET_RE.search(uri):
                    continue
                if 200 <= status < 300:
                    s["pageviews"] += 1
                    s["pages"][uri] += 1
                    ip = req.get("client_ip") or req.get("remote_ip")
                    if ip:
                        s["ips"].add(ip)  # counted below, then discarded
                    ref = (req.get("headers", {}).get("Referer") or [""])[0]
                    if ref and host not in ref:
                        s["referrers"][ref[:80]] += 1
    return sites


def render(sites, hours):
    lines = [f"Site traffic digest - last {hours}h (aggregate only, no per-visitor data kept)", ""]
    if not sites:
        lines.append("No traffic in the window.")
    for host, s in sorted(sites.items()):
        uniques = len(s["ips"])
        s["ips"] = None  # discard identifiers immediately after counting
        lines += [
            f"== {host} ==",
            f"  pageviews: {s['pageviews']}   ~unique visitors: {uniques}   "
            f"total requests: {s['requests']}   bot requests: {s['bots']}",
            "  status: " + "  ".join(f"{k}:{v}" for k, v in sorted(s["status"].items())),
            "  top pages:",
            *[f"    {n:>5}  {p}" for p, n in s["pages"].most_common(10)],
        ]
        if s["referrers"]:
            lines += ["  top referrers:", *[f"    {n:>5}  {r}" for r, n in s["referrers"].most_common(5)]]
        if s["pings"]:
            vw = "  ".join(f"{k}:{v}" for k, v in s["ping_vw"].most_common())
            th = "  ".join(f"{k}:{v}" for k, v in s["ping_theme"].most_common())
            lines += [
                f"  opt-in pings (consent-gated beacon): {s['pings']}   viewports: {vw}   themes: {th}",
                *[f"    {n:>5}  {p}" for p, n in s["ping_pages"].most_common(5)],
            ]
            if s["ping_refs"]:
                lines += ["    ping referrer domains:", *[f"    {n:>5}  {r}" for r, n in s["ping_refs"].most_common(3)]]
        lines.append("")
    return "\n".join(lines)


def main():
    args = parse_args()
    paths = args.logs or sorted(glob.glob("/var/log/caddy/*.log"))
    text = render(digest(paths, args.hours), args.hours)
    print(text)
    if args.mail:
        msg = f"Subject: [site-stats] daily traffic digest\nTo: {args.mail}\n\n{text}\n"
        try:
            subprocess.run(["sendmail", args.mail], input=msg.encode(), check=True)
        except Exception as e:  # noqa: BLE001 - report, don't crash the timer
            print(f"WARN: mail failed: {e}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
