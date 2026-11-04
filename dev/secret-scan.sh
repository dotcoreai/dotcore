#!/usr/bin/env bash
# Scans a repository's tracked files and its git history for committed secrets with gitleaks and
# trufflehog. Runs offline, never verifies a finding against its service, and prints each finding's
# location and kind without the secret. Exits 1 on a finding and 2 when a scanner fails to run.
set -euo pipefail

# Exit status both scanners are told to use for findings, so it differs from their own errors.
FINDINGS=183

main() {
  root=$(cd "${1:-.}" && pwd)
  cd "$root"

  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  status=0

  local gitleaks_options=(--config "$root/.gitleaks.toml" --no-banner --redact --log-level error
    --exit-code "$FINDINGS" --report-format json)
  # The nixpkgs trufflehog already disables its self-update and refuses the flag twice. Nix store
  # hashes have the shape of Box tokens, so that detector only reports store paths here.
  local trufflehog_options=(--no-verification --json --fail --exclude-detectors=Box)

  # Tracked files only: build outputs, dependencies and local environment files are not the
  # repository's content. jj lists the snapshotted working copy, which git's index may lag.
  local files
  if [ -d .jj ]; then
    mapfile -t files < <(jj file list --no-pager)
  else
    mapfile -t files < <(git ls-files)
  fi
  link_tree "$work/tree" "${files[@]}"

  echo "== tracked files (${#files[@]})"
  scan gitleaks-tree gitleaks dir "$work/tree" --follow-symlinks "${gitleaks_options[@]}" \
    --report-path "$work/gitleaks-tree.json"
  # shellcheck disable=SC2016 # jq variables, not shell ones
  report "$work/gitleaks-tree.json" --arg tree "$work/tree/" --arg root "$root/" \
    '.[] | "gitleaks: \(.File | ltrimstr($tree) | ltrimstr($root)):\(.StartLine) \(.RuleID)"'

  scan trufflehog-tree trufflehog filesystem "${trufflehog_options[@]}" "${files[@]}" \
    > "$work/trufflehog-tree.json"
  report "$work/trufflehog-tree.json" \
    '"trufflehog: \(.SourceMetadata.Data.Filesystem.file):\(.SourceMetadata.Data.Filesystem.line) \(.DetectorName)"'

  # Published history only: jj keeps references to every rewritten commit, which would multiply the
  # scan without adding content anyone can fetch. trufflehog takes one branch, so it scans HEAD's.
  local refs=(HEAD --branches --tags)
  echo "== git history ($(git rev-list "${refs[@]}" | wc -l) commits)"
  scan gitleaks-history gitleaks git . --log-opts="${refs[*]}" "${gitleaks_options[@]}" \
    --report-path "$work/gitleaks-history.json"
  report "$work/gitleaks-history.json" \
    '.[] | "gitleaks: \(.Commit[0:12]) \(.File):\(.StartLine) \(.RuleID)"'

  scan trufflehog-history trufflehog git "file://$root" --branch HEAD "${trufflehog_options[@]}" \
    > "$work/trufflehog-history.json"
  report "$work/trufflehog-history.json" \
    '"trufflehog: \(.SourceMetadata.Data.Git.commit[0:12]) \(.SourceMetadata.Data.Git.file):\(.SourceMetadata.Data.Git.line) \(.DetectorName)"'

  case "$status" in
    0) echo "no secrets found" ;;
    1) echo "secret scan failed: review each finding above" >&2 ;;
    *) echo "secret scan failed: a scanner did not run; see its log above" >&2 ;;
  esac
  exit "$status"
}

# Runs one scanner, keeping its log; a finding sets status 1 and any other failure status 2.
scan() {
  local name=$1 code=0
  shift
  "$@" 2> "$work/$name.log" || code=$?
  if [ "$code" -eq "$FINDINGS" ]; then
    [ "$status" -ge 1 ] || status=1
  elif [ "$code" -ne 0 ]; then
    echo "$name: exited with $code" >&2
    tail -n 20 "$work/$name.log" >&2
    status=2
  fi
}

# Prints a scanner's findings with a jq filter, selecting only locations and kinds.
report() {
  local file=$1
  shift
  [ -s "$file" ] || return 0
  jq -r "$@" "$file"
}

# Mirrors the tracked files as links in one directory, since gitleaks scans a single directory.
link_tree() {
  local tree=$1 file
  shift
  mkdir "$tree"
  for file in "$@"; do
    [ -f "$file" ] || continue
    mkdir -p "$tree/$(dirname "$file")"
    ln -s "$PWD/$file" "$tree/$file"
  done
}

main "$@"
