#!/usr/bin/env bash
# Every skill under plugin/skills/ is a hand-maintained COPY of its .agents/skills/ source (#1075).
# .claude/skills and .codex/skills are symlinks into .agents, so those always agree; the plugin copy
# is the one that ships to users, and nothing else in the repo notices when the two diverge — no
# build, test or lint reads them. #1074 had to edit both and the drift was caught only by an
# unrelated `diff -q`. This is that diff, run on every PR that touches either side.
#
# Direction: plugin/skills/ defines the SHIPPED set, so the loop walks it. The reverse is not an
# error — repository-only contributor skills (e.g. rust-modern-style) live under .agents/skills and
# are deliberately absent from the installer (skills/README.md); they're listed here, not failed on.
# Pairs are discovered, never listed: a hardcoded list would itself go stale the next time a skill is
# added, which is the defect this check exists to prevent.
set -u
ROOT="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
SRC_ROOT="$ROOT/.agents/skills"
SHIPPED_ROOT="$ROOT/plugin/skills"
step() { echo; echo "### $*"; }
pass() { echo "PASS: $*"; }
# Unlike the install scripts, fail() does NOT exit: with several skills it is worth naming EVERY
# diverged pair in one run rather than making the author re-run CI per skill.
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

[ -d "$SHIPPED_ROOT" ] || { echo "FAIL: no shipped skills directory at $SHIPPED_ROOT" >&2; exit 1; }
[ -d "$SRC_ROOT" ] || { echo "FAIL: no skill source directory at $SRC_ROOT" >&2; exit 1; }

step "diff each shipped skill against its source"
pairs=0
for shipped in "$SHIPPED_ROOT"/*/; do
  [ -d "$shipped" ] || continue # unexpanded glob: no shipped skills at all
  name="$(basename "$shipped")"
  src="$SRC_ROOT/$name"
  pairs=$((pairs + 1))
  if [ ! -d "$src" ]; then
    fail "plugin/skills/$name ships with no source at .agents/skills/$name"
    continue
  fi
  # -r so reference/ files and any future asset count too, and so a file present on only one side
  # is reported; -u because the diff is the whole point of the failure message.
  if diff -ru "$src" "$shipped"; then
    pass ".agents/skills/$name == plugin/skills/$name"
  else
    fail ".agents/skills/$name and plugin/skills/$name have diverged (diff above: --- source, +++ shipped)"
  fi
done

# A checkout with no pairs left to compare means the layout moved and this check silently stopped
# testing anything — the exact failure mode it guards against.
[ "$pairs" -gt 0 ] || { echo "FAIL: no skills found under $SHIPPED_ROOT" >&2; exit 1; }

step "repository-only skills (no shipped copy — informational)"
for src in "$SRC_ROOT"/*/; do
  [ -d "$src" ] || continue
  name="$(basename "$src")"
  [ -d "$SHIPPED_ROOT/$name" ] || echo "  .agents/skills/$name (not in the installer bundle)"
done

echo
if [ "$failures" -gt 0 ]; then
  echo "skill parity: $failures of $pairs shipped skill(s) differ from their source." >&2
  echo "Fix by copying the authoritative side over the other — both must be byte-identical." >&2
  exit 1
fi
echo "skill parity verification: OK ($pairs shipped skill(s) match their source)"
