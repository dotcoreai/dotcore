#!/bin/sh
set -eu

inference_url=${HERMES_INFERENCE_BASE_URL:-}
model=${HERMES_MODEL:-}
config=/opt/data/config.yaml
temporary=/opt/data/.config.yaml.$$

# Dotcore resolves both values; these checks keep them safe inside the quoted YAML below.
case $inference_url in
*[!A-Za-z0-9.:/-]*) inference_url= ;;
http://?*/v1 | https://?*/v1) ;;
*) inference_url= ;;
esac
if [ -z "$inference_url" ]; then
  printf '%s\n' 'HERMES_INFERENCE_BASE_URL must be an HTTP(S) inference API URL ending in /v1.' >&2
  exit 64
fi

# An empty model means no inference route is linked; Hermes then has no default model.
case $model in
*[!a-z0-9.-]*)
  printf '%s\n' 'HERMES_MODEL must be empty or an inference route name.' >&2
  exit 64
  ;;
esac

if [ ! -d /opt/data ]; then
  printf '%s\n' '/opt/data must be a writable directory.' >&2
  exit 73
fi

# Hermes lists a provider's models from its /models endpoint only when the provider has an API key.
# The Dotcore inference API requires no key, so this fixed non-secret placeholder enables that list.
placeholder_api_key=dotcore-no-key

umask 022
trap 'rm -f "$temporary"' EXIT HUP INT TERM
{
  cat <<EOF
gateway:
  api_server:
    enabled: false
auxiliary:
  title_generation:
    enabled: false
providers:
  dotcore:
    base_url: "$inference_url"
    api_key: "$placeholder_api_key"
EOF
  if [ -n "$model" ]; then
    cat <<EOF
    models:
      - "$model"
model:
  provider: dotcore
  default: "$model"
EOF
  else
    cat <<EOF
model:
  provider: dotcore
EOF
  fi
} >"$temporary"
mv -f "$temporary" "$config"
trap - EXIT HUP INT TERM
exec "$@"
