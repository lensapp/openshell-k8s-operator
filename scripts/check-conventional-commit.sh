#!/bin/sh
# Validate a commit subject against Conventional Commits, the format
# release-please derives versions + changelog from. Shared by the commit-msg
# hook (.githooks/commit-msg) and the CI backstop (.github/workflows/ci.yml) so
# the two can't drift. Takes the subject line as $1.
set -e

subject=${1:-}
pattern='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\([a-z0-9._/-]+\))?!?: .+'

if printf '%s' "$subject" | grep -Eq "$pattern"; then
  exit 0
fi

echo "✗ Not a Conventional Commit subject:" >&2
echo "    ${subject:-<empty>}" >&2
echo "" >&2
echo "  Expected: <type>(<optional scope>): <description>" >&2
echo "  Types:    feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert" >&2
echo "  Example:  feat(provider): configure gateway-minted refresh" >&2
echo "" >&2
echo "  Only feat/fix (and a ! / BREAKING CHANGE) cut a release, so use the" >&2
echo "  right type or release-please will skip your change." >&2
exit 1
