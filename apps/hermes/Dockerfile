# Hermes Agent v0.15.0 (2026.5.28), pinned to its immutable multi-platform index.
# Update this digest only after reviewing the selected upstream release and rerunning
# `just apps hermes image-test` on each native platform.
FROM nousresearch/hermes-agent@sha256:497d818208838f2cd3fb992b999450856acc725c6d97c55962289f1fd92b8b91

# Preserve the upstream entrypoint so its volume preparation, gateway
# supervision, and non-root execution remain in effect. The catalog arguments invoke
# this wrapper, which owns only /opt/data/config.yaml before launching Hermes.
COPY --chmod=0555 start.sh /usr/local/bin/dotcore-hermes-start

ENV HERMES_GATEWAY_BOOTSTRAP_STATE=running
