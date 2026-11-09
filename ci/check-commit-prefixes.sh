#!/usr/bin/env bash
# Fails when a commit in a git revision range still carries a prefix that keeps it off the main
# branch: a change pending its author's review, or a private one that is never pushed. Prints each
# such commit's short hash and subject. Exits 1 on a match and with git's status when the range is
# invalid.
set -euo pipefail

# Subject prefixes that block a merge.
PREFIXES='(unreviewed|private): '

main() {
  local range=$1

  # Listed before filtering, so a git error stops the script instead of reading as no match.
  local log
  log=$(git log --format='%h %s' "$range")

  local blocked
  blocked=$(printf '%s\n' "$log" | grep -E "^[0-9a-f]+ $PREFIXES" || true)

  if [[ -z $blocked ]]; then
    echo "no unreviewed or private commits in $range"
    return 0
  fi

  echo "commits still marked unreviewed or private in $range:"
  printf '%s\n' "$blocked"
  # GitHub Actions shows this line as an error annotation on the run.
  echo "::error::this range holds unreviewed or private commits; review and reword them, or drop them, before merging"
  return 1
}

main "$@"
