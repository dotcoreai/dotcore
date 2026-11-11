# Hermes Agent v0.23.1, pinned to its immutable multi-platform index.
# Update this digest only after reviewing the selected upstream release and rerunning
# `just apps hermes image-test` on each native platform.
FROM nousresearch/hermes-agent@sha256:276d44685c65c0cb2a25f190b63c65c8906f9dc76486fb1c05ae71b5826bda08

# Preserve the upstream entrypoint so its volume preparation, gateway
# supervision, and non-root execution remain in effect. The catalog arguments invoke
# this wrapper, which owns only /opt/data/config.yaml before launching Hermes.
COPY --chmod=0555 start.sh /usr/local/bin/dotcore-hermes-start

ENV HERMES_GATEWAY_BOOTSTRAP_STATE=running
