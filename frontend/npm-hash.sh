# Reads the npm dependency hash the Nix frontend build pins, and computes the one package-lock.json
# needs. Sourced by the frontend recipes, from the frontend folder.

NPM_HASH_FILE=../nix/frontend.nix

# The hash pinned in the Nix build's npm dependency fetch.
npm_hash_committed() {
  local hash
  hash=$(sed -n 's/^ *hash = "\(sha256-[^"]*\)";$/\1/p' "$NPM_HASH_FILE")
  if [ -z "$hash" ] || [ "$(printf '%s\n' "$hash" | wc -l)" -ne 1 ]; then
    echo "expected exactly one npm dependency hash in $NPM_HASH_FILE" >&2
    return 1
  fi
  printf '%s\n' "$hash"
}

# The hash of package-lock.json, computed with the flake's nixpkgs so it matches the Nix build.
npm_hash_locked() {
  local log
  log=$(mktemp)
  # The tool prints download progress on stderr; show it only when it fails.
  if ! nix shell --inputs-from .. nixpkgs#prefetch-npm-deps -c prefetch-npm-deps package-lock.json 2> "$log"; then
    cat "$log" >&2
    rm -f "$log"
    return 1
  fi
  rm -f "$log"
}
