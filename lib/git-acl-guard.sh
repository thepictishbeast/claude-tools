#!/usr/bin/env bash
# Keep paul's git repos usable when root writes into them.
#
# The failure this prevents, seen twice in one night: a process running as
# root does a git operation in one of paul's repos — a commit, a fetch —
# and leaves objects owned root:root 0644. paul then cannot read his own
# HEAD commit, and git reports:
#
#     fatal: your current branch appears to be broken
#
# which sounds like corruption and is not. The refs are perfect; the
# object is simply unreadable. It cost a whole session of leaving a repo
# alone under the belief it was damaged.
#
# Re-chowning after the fact is whack-a-mole — 3,968 files across 29 repos
# on the first sweep, and it came back within two hours. The durable fix
# is to make ownership stop mattering:
#
#   setgid on every directory   -> new files inherit the `paul` group
#                                  whoever creates them
#   default ACL g:paul:rwX      -> and are group-writable, not merely
#                                  group-owned, which is the half that
#                                  setgid alone does not give you
#
# Root still writes root-owned files. paul can now read and rewrite them,
# which is all git needs.
#
# This runs periodically because new repos appear, and a repo cloned
# tomorrow has none of this.
#
#   git-acl-guard.sh [--root DIR] [--check]
set -uo pipefail

ROOT="${GIT_ACL_ROOT:-/home/paul/projects}"
OWNER="${GIT_ACL_OWNER:-paul}"
CHECK=0
while [ $# -gt 0 ]; do
  case "$1" in
    --root)  ROOT="$2"; shift 2 ;;
    --check) CHECK=1; shift ;;
    *) echo "git-acl-guard: unknown argument $1" >&2; exit 2 ;;
  esac
done

command -v setfacl >/dev/null 2>&1 || { echo "git-acl-guard: setfacl not installed" >&2; exit 2; }

hardened=0; already=0; unreadable=0
for d in "$ROOT"/*/.git; do
  [ -d "$d" ] || continue
  repo=$(basename "$(dirname "$d")")

  # Already carrying the default ACL? Then it only needs a top-up if
  # something new has appeared without it.
  if getfacl -p "$d" 2>/dev/null | grep -q "^default:group:${OWNER}:rwx"; then
    already=$((already+1))
  else
    [ "$CHECK" = 1 ] && { echo "  would harden: $repo"; continue; }
    chgrp -R "$OWNER" "$d" 2>/dev/null
    chmod -R g+rwX "$d" 2>/dev/null
    find "$d" -type d -exec chmod g+s {} + 2>/dev/null
    setfacl -R -m "g:${OWNER}:rwX" -d -m "g:${OWNER}:rwX" "$d" 2>/dev/null
    hardened=$((hardened+1))
    echo "  hardened: $repo"
  fi

  # The check that actually matters: can the owner read HEAD? This is the
  # symptom users see, so test it rather than inferring it from modes.
  if ! sudo -u "$OWNER" git -C "$(dirname "$d")" --no-optional-locks \
       rev-parse HEAD >/dev/null 2>&1; then
    # A repo with no commits yet is fine; a repo with a HEAD that cannot
    # be resolved is the failure.
    if [ -s "$d/HEAD" ]; then
      unreadable=$((unreadable+1))
      echo "  STILL BROKEN: $repo — $OWNER cannot resolve HEAD" >&2
    fi
  fi
done

echo "git-acl-guard: $hardened hardened, $already already protected, $unreadable still broken"
[ "$unreadable" -eq 0 ]
