# shellcheck shell=bash
# Fake host Nix for loader tests: `build` succeeds when every requested path exists (as the test's
# realized inputs do) unless $FAKE_STATE/nix-fails exists, linking the first path at its out-link;
# hashing returns a fixed value, and each store addition a new path, as a nondeterministic Docker
# build would.

state=$FAKE_STATE
echo "nix $*" >>"$state/calls"

case "$1" in
  build)
    shift
    paths=() link=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --out-link)
          shift
          link=$1
          ;;
        *) paths+=("$1") ;;
      esac
      shift
    done
    for path in "${paths[@]}"; do
      if [ -e "$state/nix-fails" ] || [ ! -e "$path" ]; then
        echo "error: path '$path' is required, but there is no substituter that can build it" >&2
        exit 1
      fi
    done
    [ -z "$link" ] || ln -sfn "${paths[0]}" "$link"
    ;;
  hash) echo "sha256-fakehash" ;;
  store)
    # Stands in for the added store path, which the store build mode then roots.
    stored="$state/fake-stored"
    count=1
    while [ -e "$stored" ]; do
      count=$((count + 1))
      stored="$state/fake-stored-$count"
    done
    mkdir -p "$stored"
    echo "$stored"
    ;;
  *)
    echo "fake nix: unexpected call: $*" >&2
    exit 2
    ;;
esac
