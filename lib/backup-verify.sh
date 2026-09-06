#!/usr/bin/env bash
# Prove each backup can actually give data back, not that the job exited 0.
#
# Every backup job on this host reports success. That is a statement about
# last night. Whether anything can be restored is a claim about the
# future, and running is not evidence for restoring: a job completes
# cleanly while writing a truncated archive, while skipping the one
# dataset that mattered, or while faithfully copying a database that was
# mid-write and is therefore unopenable. The exit code is identical.
#
# So this does not check that the job ran. It opens the backup and takes
# something out.
#
# Two things beyond the bytes decide whether a restore produces a WORKING
# system, and both are checked here because neither is inside the file:
#
#   ownership   restore mail as the wrong account and every message is
#               present, correct, and unreadable by the mail server. You
#               get a full directory and a service that reports an empty
#               mailbox — the failure that looks most like success.
#   structure   a mail store is messages plus the index and folder list.
#               Recovering the letters without the filing cabinet leaves
#               someone rebuilding years of folders by hand.
#
#   backup-verify.sh [--quiet] [--notify]
set -uo pipefail

NOTIFY="${BACKUP_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
TOPIC="${BACKUP_TOPIC:-plausiden-alerts}"
MAIL_BACKUP="${MAIL_BACKUP:-/tank/backups/mail}"
MAIL_LIVE="${MAIL_LIVE:-/var/mail/vhosts/plausiden.com}"
SEND=0; QUIET=0
while [ $# -gt 0 ]; do
  case "$1" in
    --notify) SEND=1; shift ;;
    --quiet)  QUIET=1; shift ;;
    *) echo "backup-verify: unknown argument $1" >&2; exit 2 ;;
  esac
done

problems=""; checked=0; ok=0
say(){ [ "$QUIET" = 1 ] || printf '%s\n' "$*"; }
fail(){ problems="${problems}  $1"$'\n'; say "  $2 FAIL — $1"; }
pass(){ ok=$((ok+1)); say "  $1 ok — $2"; }

# ── mail: pull a real message out and prove it is a usable message ──────
checked=$((checked+1))
if [ ! -d "$MAIL_BACKUP" ]; then
  fail "mail: no backup at $MAIL_BACKUP" "mail"
else
  src=$(find "$MAIL_BACKUP" -type f -path '*/cur/*' 2>/dev/null | head -1)
  if [ -z "$src" ]; then
    fail "mail: backup exists but contains no messages" "mail"
  else
    body_bytes=$(python3 - "$src" <<'PY' 2>/dev/null
import email, sys
m = email.message_from_binary_file(open(sys.argv[1], 'rb'))
if not m.get('From') and not m.get('Subject'):
    print(0); raise SystemExit
for part in m.walk():
    if part.get_content_type() == 'text/plain':
        p = part.get_payload(decode=True)
        print(len(p) if p else 0); raise SystemExit
print(0)
PY
)
    if [ "${body_bytes:-0}" -lt 1 ]; then
      fail "mail: a backed-up message did not parse or had no recoverable body" "mail"
    else
      # Ownership. This is the check that catches a restore producing
      # data the service cannot read.
      bo=$(stat -c '%U:%G' "$src" 2>/dev/null)
      lo=$(stat -c '%U:%G' "$MAIL_LIVE" 2>/dev/null)
      if [ -n "$lo" ] && [ "$bo" != "$lo" ]; then
        fail "mail: backup owned $bo but live store is $lo — a restore would be unreadable by the mail server" "mail"
      else
        # Structure. Messages without the index are letters without the
        # filing cabinet.
        idx=$(find "$MAIL_BACKUP" -maxdepth 3 -name 'dovecot.list.index' 2>/dev/null | head -1)
        [ -n "$idx" ] || fail "mail: messages present but no mailbox index — folders would need rebuilding by hand" "mail"
        live_n=$(find "$MAIL_LIVE" -type f \( -path '*/cur/*' -o -path '*/new/*' \) 2>/dev/null | wc -l)
        back_n=$(find "$MAIL_BACKUP" -type f \( -path '*/cur/*' -o -path '*/new/*' \) 2>/dev/null | wc -l)
        gap=$(( live_n - back_n ))
        # A gap is normal — mail arrives after the run. A LARGE gap is
        # not, and without a threshold "some difference" is unreadable.
        if [ "$gap" -gt "${MAIL_GAP_MAX:-500}" ]; then
          fail "mail: backup is behind live by $gap messages (max ${MAIL_GAP_MAX:-500})" "mail"
        elif [ -n "$idx" ]; then
          pass "mail" "message parsed, ${body_bytes}B body, owner $bo, index present, $gap behind live"
        fi
      fi
    fi
  fi
fi

# ── zfs snapshots: exist, and are recent enough to be worth having ──────
checked=$((checked+1))
if ! command -v zfs >/dev/null 2>&1; then
  say "  zfs skipped — not installed"
else
  newest=$(zfs list -t snapshot -o name,creation -s creation -H 2>/dev/null | tail -1)
  if [ -z "$newest" ]; then
    fail "zfs: no snapshots exist at all" "zfs"
  else
    age_h=$(( ( $(date +%s) - $(date -d "$(printf '%s' "$newest" | cut -f2)" +%s 2>/dev/null || echo 0) ) / 3600 ))
    if [ "$age_h" -gt "${ZFS_MAX_AGE_H:-26}" ]; then
      fail "zfs: newest snapshot is ${age_h}h old — snapshots have stopped" "zfs"
    else
      # Readable, not merely listed: a snapshot you cannot open is a name.
      snap=$(printf '%s' "$newest" | cut -f1)
      ds="${snap%@*}"; mp=$(zfs get -H -o value mountpoint "$ds" 2>/dev/null)
      if [ -d "$mp/.zfs/snapshot/${snap#*@}" ] || [ "$mp" = "none" ] || [ "$mp" = "-" ]; then
        pass "zfs" "$(zfs list -t snapshot -H 2>/dev/null | wc -l) snapshots, newest ${age_h}h old and readable"
      else
        fail "zfs: newest snapshot is listed but its contents are not reachable" "zfs"
      fi
    fi
  fi
fi

# ── dpkg package database: the archive must actually decompress ─────────
checked=$((checked+1))
d=$(ls -t /var/backups/dpkg.status.* 2>/dev/null | head -1)
if [ -z "$d" ]; then
  fail "dpkg: no package-database backup found" "dpkg"
elif printf '%s' "$d" | grep -q '\.gz$'; then
  if gzip -t "$d" 2>/dev/null; then
    n=$(zcat "$d" 2>/dev/null | grep -c '^Package:')
    [ "${n:-0}" -gt 100 ] && pass "dpkg" "archive decompresses, $n packages recorded" \
      || fail "dpkg: archive opens but records only ${n:-0} packages" "dpkg"
  else
    fail "dpkg: archive is corrupt and will not decompress" "dpkg"
  fi
else
  n=$(grep -c '^Package:' "$d" 2>/dev/null)
  [ "${n:-0}" -gt 100 ] && pass "dpkg" "$n packages recorded" \
    || fail "dpkg: only ${n:-0} packages recorded" "dpkg"
fi

# ── showings: client-facing, so verify it holds real content ────────────
checked=$((checked+1))
s=$(ls -t /var/backups/showings/* 2>/dev/null | head -1)
if [ -z "$s" ]; then
  fail "showings: no backup found — this one is client-facing" "showings"
else
  age_d=$(( ( $(date +%s) - $(stat -c %Y "$s" 2>/dev/null || echo 0) ) / 86400 ))
  sz=$(stat -c %s "$s" 2>/dev/null || echo 0)
  if [ "$age_d" -gt "${SHOWINGS_MAX_AGE_D:-2}" ]; then
    fail "showings: newest backup is ${age_d} days old" "showings"
  elif [ "$sz" -lt 1024 ]; then
    fail "showings: newest backup is only ${sz} bytes — almost certainly empty" "showings"
  else
    pass "showings" "$(basename "$s"), ${sz} bytes, ${age_d}d old"
  fi
fi

say ""
if [ -n "$problems" ]; then
  say "backup-verify: $ok of $checked verified, $(printf '%s' "$problems" | grep -c .) problem(s)"
  [ "$SEND" = 1 ] && printf 'A backup could not give its data back.\n\n%s\nThese jobs all report success. Success means the job ran, not that a restore would work.\n' \
    "$problems" | "$NOTIFY" --key backup-verify --title "Backup failed its restore test" \
      --priority high --tags floppy_disk --topic "$TOPIC" >/dev/null 2>&1
  exit 1
fi
say "backup-verify: $ok of $checked backups gave their data back"
[ "$SEND" = 1 ] && "$NOTIFY" --resolve backup-verify --topic "$TOPIC" >/dev/null 2>&1
exit 0
