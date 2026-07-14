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
    p.add_argument("--heatmap-out", metavar="DIR", help="write per-page click-overlay SVGs here")
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
        # Interaction pings (k=c clicks on a 2% grid, k=s quartile scroll):
        # keyed by (page, x, y) / (page, depth) - see mom-site docs/HEATMAPS.md.
        "clicks": Counter(), "scroll": Counter(),
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
                        kind = q.get("k", [""])[0]
                        page = q.get("p", [""])[0][:80]
                        if kind == "c":
                            try:
                                x, y = int(q.get("x", ["-1"])[0]), int(q.get("y", ["-1"])[0])
                            except ValueError:
                                continue
                            if page and 0 <= x <= 100 and 0 <= y <= 100:
                                s["clicks"][(page, x, y)] += 1
                        elif kind == "s":
                            d = q.get("d", [""])[0]
                            if page and d in ("25", "50", "75", "100"):
                                s["scroll"][(page, int(d))] += 1
                        elif not kind:
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
        if s["clicks"]:
            per_page = Counter()
            for (page, _x, _y), n in s["clicks"].items():
                per_page[page] += n
            lines += ["  interaction clicks (2% grid):",
                      *[f"    {n:>5}  {p}" for p, n in per_page.most_common(5)]]
        if s["scroll"]:
            depth_pages = sorted({page for page, _d in s["scroll"]})
            lines.append("  scroll depth (quartile reached):")
            for page in depth_pages[:5]:
                dist = "  ".join(f"{d}%:{s['scroll'][(page, d)]}" for d in (25, 50, 75, 100) if s["scroll"][(page, d)])
                lines.append(f"    {page}  {dist}")
        lines.append("")
    return "\n".join(lines)


def render_heatmaps(sites, outdir):
    """One SVG per page with clicks: 100x100 viewBox, one 2x2 cell per grid
    point, opacity scaled to the busiest cell. Self-contained files - open in
    any browser, overlay by eye against the page at the same width."""
    import pathlib
    out = pathlib.Path(outdir)
    out.mkdir(parents=True, exist_ok=True)
    written = 0
    for host, s in sites.items():
        pages = defaultdict(Counter)
        for (page, x, y), n in s["clicks"].items():
            pages[page][(x, y)] += n
        for page, cells in pages.items():
            peak = max(cells.values())
            body = "".join(
                f'<rect x="{x}" y="{y}" width="2" height="2" fill="#c22" '
                f'fill-opacity="{0.15 + 0.85 * n / peak:.2f}"><title>{n}</title></rect>'
                for (x, y), n in sorted(cells.items())
            )
            slug = re.sub(r"[^a-z0-9]+", "-", (host + page).lower()).strip("-") or "root"
            (out / f"{slug}.svg").write_text(
                f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">'
                f'<rect width="100" height="100" fill="#fff"/>{body}'
                f'<text x="1" y="3" font-size="2.5" fill="#555">{host}{page} - '
                f'{sum(cells.values())} clicks</text></svg>\n'
            )
            written += 1
    return written


def main():
    args = parse_args()
    paths = args.logs or sorted(glob.glob("/var/log/caddy/*.log"))
    sites = digest(paths, args.hours)
    if args.heatmap_out:
        n = render_heatmaps(sites, args.heatmap_out)
        print(f"heatmaps: {n} overlay(s) written to {args.heatmap_out}", file=sys.stderr)
    text = render(sites, args.hours)
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
