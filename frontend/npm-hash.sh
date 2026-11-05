# The npm dependency hash pinned by the Nix frontend build, and the one package-lock.json needs.

NPM_HASH_FILE=../nix/frontend.nix

npm_hash_committed() {
  local hash
  hash=$(sed -n 's/^ *hash = "\(sha256-[^"]*\)";$/\1/p' "$NPM_HASH_FILE")
  if [ -z "$hash" ] || [ "$(printf '%s\n' "$hash" | wc -l)" -ne 1 ]; then
    echo "expected exactly one npm dependency hash in $NPM_HASH_FILE" >&2
    return 1
  fi
  printf '%s\n' "$hash"
}

# Uses the flake's nixpkgs, so the hash matches the Nix build.
npm_hash_locked() {
  local log
  log=$(mktemp)
  # Progress goes to stderr; show it only on failure.
  if ! nix shell --inputs-from .. nixpkgs#prefetch-npm-deps -c prefetch-npm-deps package-lock.json 2> "$log"; then
    cat "$log" >&2
    rm -f "$log"
    return 1
  fi
  rm -f "$log"
}
