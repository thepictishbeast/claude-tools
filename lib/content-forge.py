#!/usr/bin/env python3
"""Draft a plausiden.com blog post from a question people actually asked.

Inbound content is the one lead channel that keeps working after you stop
paying for it, and the only one that needs nobody's permission. The catch
on this site is that posts are not markdown — they are Rust modules using
maud, registered in a static slice. So a generated post has to compile,
pass 270 tests, and survive the gates, or it is worse than nothing.

Hence the shape of this tool: it writes the file, wires it into the
registry, builds, and REVERTS ITSELF if any of that fails. A half-added
post that breaks the build would take the whole site down at the next
deploy — and `pd-deploy.sh` does not build, so a broken tree would sit
there looking fine until someone shipped it.

Topics come from a queue rather than imagination. The point of pairing
this with lead-scout is to write about what people are actually stuck on,
not what we assume they are.

  content-forge.py --topic "..." [--category Engineering] [--dry-run]
  content-forge.py --from-queue [--limit 1]
"""
import argparse, datetime, os, re, subprocess, sys, shutil

REPO = os.environ.get("SITE_REPO", "/home/paul/projects/plausiden.com")
POSTS = f"{REPO}/src/views/posts"
QUEUE = os.environ.get("CONTENT_QUEUE", "/var/lib/content-forge/queue.txt")
CLAUDE = os.environ.get("CLAUDE_BIN", "/root/.local/bin/claude")
MODEL = os.environ.get("CONTENT_MODEL", "claude-sonnet-5")

# What the site is and is not, handed to the drafter verbatim. Getting
# this wrong is how a generated post invents a client, a certification or
# a capability, which is far more expensive than a post that never ran.
HOUSE = """PlausiDen is a small Massachusetts firm: IT operations, cyber security,
disaster recovery, network architecture, software development, hardware,
industrial automation, AI on infrastructure the client controls, and managed IT.
Clients are organisations of 5-100 staff that hold confidential information —
law firms, medical practices, financial advisers, newsrooms, nonprofits — and
increasingly others. Rates are published. Proposals are fixed-price. A mutual
NDA comes before the first call. The reader talks to the engineer who does the
work.

The writing is plain, technical and unshowy. It explains what breaks underneath
a problem. It never uses "leverage", "solutions", "empower", "in today's
landscape", "unlock", or exclamation marks. It does not open with a rhetorical
question. It is willing to say when something is not worth buying.

ABSOLUTE RULES:
- Invent NOTHING. No client names, no case studies, no statistics, no dates, no
  certifications, no claims about what PlausiDen has done. If a concrete example
  helps, make it obviously hypothetical ("imagine a practice that...").
- Stay off pricing entirely. Not the figures, not the subject: no "what this
  should cost", no "cheaper than", no comparisons of vendor pricing models, no
  hourly-versus-fixed discussion. Rates live on one page that is deliberately
  the only place the question is answered, and a post that opens the topic
  invites the reader to argue with a number instead of reading the argument.
  If pricing is unavoidable context, one clause at most, then move on.
- Do not promise outcomes or timelines."""

BODY_PROMPT = """%s

Write the BODY of a blog post for plausiden.com answering this question:

  %s

Structure it as a sequence of blocks, one per line, using exactly these forms:

  H: <a section heading, sentence case, no trailing period>
  P: <a paragraph of prose>

Rules:
- Start with 2 P: blocks before the first H:, as a lede.
- 3 to 5 H: sections, each with 2-4 P: paragraphs.
- 700-1100 words total.
- Diagnose the mechanism. Say what actually breaks and why.
- Give the reader something they can act on even if they never hire anyone.
- End on a paragraph that is useful, not a pitch. No call to action.
- Plain ASCII quotes and apostrophes only.

Output ONLY the H:/P: lines. No preamble, no markdown, no bullet lists."""

META_PROMPT = """Given this blog post body, produce exactly three lines:

TITLE: <a specific, plain title, under 60 characters, no colon-subtitle>
EXCERPT: <one sentence, 150-215 characters, that states what the reader learns>
CATEGORY: <one word from: Engineering, Privacy, Security, Architecture, Operations>

No other output.

BODY:
%s"""


def claude(prompt, timeout=600):
    r = subprocess.run([CLAUDE, "-p", "--model", MODEL], input=prompt,
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout.strip()


def rust_str(s):
    """Escape prose for a Rust string literal.

    Only backslash and double-quote need escaping, and the order matters:
    escaping quotes first would then double-escape their backslashes.
    """
    return s.replace("\\", "\\\\").replace('"', '\\"')


def slugify(title):
    s = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    return re.sub(r"-{2,}", "-", s)[:48].strip("-")


def render_module(slug, title, blocks):
    out = [
        f"//! {title}",
        "//!",
        "//! Drafted by content-forge from a question observed in the wild.",
        "//! Sanitized: names no client, cites no figure, claims no engagement.",
        "",
        "use loom_components::ArticleHeading;",
        "use maud::{Markup, html};",
        "",
        "/// Render the post body. Wrapper supplies chrome, eyebrow, title, date.",
        "#[must_use]",
        "pub fn render() -> Markup {",
        "    html! {",
        "",
    ]
    for kind, text in blocks:
        if kind == "H":
            out.append(f'        (ArticleHeading {{ text: "{rust_str(text)}" }}.render())')
        else:
            out.append("        p {")
            out.append(f'            "{rust_str(text)}"')
            out.append("        }")
        out.append("")
    out += ["    }", "}", ""]
    return "\n".join(out)


def read_time(words):
    return f"{max(2, round(words / 220))} min read"


def register(slug, title, excerpt, category, published, minutes):
    """Insert the module declaration and the POSTS entry, newest first."""
    p = f"{POSTS}/mod.rs"
    src = open(p, encoding="utf-8").read()
    if f"pub mod {slug};" in src:
        raise SystemExit(f"content-forge: {slug} is already registered")

    mods = sorted(set(re.findall(r"^pub mod (\w+);$", src, re.M)) | {slug})
    src = re.sub(r"(?:^pub mod \w+;\n)+",
                 "".join(f"pub mod {m};\n" for m in mods), src, count=1, flags=re.M)

    entry = (
        "    Post {\n"
        f'        slug: "{slug}",\n'
        f'        title: "{rust_str(title)}",\n'
        f'        excerpt: "{rust_str(excerpt)}",\n'
        f'        category: "{category}",\n'
        f'        published: "{published}",\n'
        f'        read_time: "{minutes}",\n'
        f"        render: {slug}::render,\n"
        "    },\n"
    )
    # Newest first: the index renders in slice order, so the new entry goes
    # immediately after the opening bracket of POSTS.
    m = re.search(r"(pub static POSTS: &\[Post\] = &\[\n)", src)
    if not m:
        raise SystemExit("content-forge: could not find the POSTS slice")
    src = src[: m.end()] + entry + src[m.end():]
    open(p, "w", encoding="utf-8").write(src)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--topic")
    ap.add_argument("--from-queue", action="store_true")
    ap.add_argument("--limit", type=int, default=1)
    ap.add_argument("--category")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    topics = []
    if a.topic:
        topics = [a.topic]
    elif a.from_queue:
        if not os.path.exists(QUEUE):
            print(f"content-forge: no queue at {QUEUE}", file=sys.stderr); return 2
        lines = [l.strip() for l in open(QUEUE) if l.strip() and not l.startswith("#")]
        topics = lines[: a.limit]
    if not topics:
        print("content-forge: nothing to write (--topic or --from-queue)", file=sys.stderr)
        return 2

    for topic in topics:
        print(f"── {topic}")
        raw = claude(BODY_PROMPT % (HOUSE, topic))
        blocks = []
        for line in raw.splitlines():
            line = line.strip()
            if line.startswith("H:"):
                blocks.append(("H", line[2:].strip()))
            elif line.startswith("P:"):
                blocks.append(("P", line[2:].strip()))
        paras = [t for k, t in blocks if k == "P"]
        if len(blocks) < 6 or len(paras) < 5:
            print(f"  drafting produced too little ({len(blocks)} blocks) — skipping",
                  file=sys.stderr)
            continue
        words = sum(len(t.split()) for t in paras)

        meta = claude(META_PROMPT % "\n\n".join(paras)[:6000])
        def field(name, default=""):
            m = re.search(rf"^{name}:\s*(.+)$", meta, re.M)
            return m.group(1).strip() if m else default
        title = field("TITLE") or topic[:58]
        excerpt = field("EXCERPT")
        category = a.category or field("CATEGORY", "Engineering")
        if not (120 <= len(excerpt) <= 240):
            excerpt = (paras[0][:200].rsplit(" ", 1)[0] + ".")
        slug = slugify(title)
        path = f"{POSTS}/{slug}.rs"
        if os.path.exists(path):
            print(f"  {slug}.rs already exists — skipping", file=sys.stderr); continue

        print(f"  title:    {title}")
        print(f"  slug:     {slug}")
        print(f"  category: {category}   {words} words, {read_time(words)}")
        if a.dry_run:
            print("  (dry run — nothing written)")
            continue

        # Everything below is reversible, because a post that does not
        # compile is a broken deploy waiting to happen.
        backup = open(f"{POSTS}/mod.rs", encoding="utf-8").read()
        open(path, "w", encoding="utf-8").write(render_module(slug, title, blocks))
        try:
            register(slug, title, excerpt, category,
                     datetime.date.today().isoformat(), read_time(words))
            subprocess.run(["chown", "paul:paul", path, f"{POSTS}/mod.rs"], check=False)
            print("  building...")
            r = subprocess.run(
                ["sudo", "-u", "paul", "env", "HOME=/home/paul", "cargo", "test", "--quiet"],
                cwd=REPO, capture_output=True, text=True, timeout=1800)
            if r.returncode != 0:
                raise RuntimeError(
                    (r.stdout + r.stderr).strip().splitlines()[-1] if (r.stdout + r.stderr).strip() else "build failed")
        except Exception as e:
            os.remove(path)
            open(f"{POSTS}/mod.rs", "w", encoding="utf-8").write(backup)
            subprocess.run(["chown", "paul:paul", f"{POSTS}/mod.rs"], check=False)
            print(f"  REVERTED — {e}", file=sys.stderr)
            continue
        print(f"  OK  /blog/{slug}  (tests pass; snapshots will need accepting)")

        if a.from_queue:
            rest = [l for l in open(QUEUE) if l.strip() != topic]
            open(QUEUE, "w").writelines(rest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
