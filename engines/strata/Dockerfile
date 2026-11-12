# Strata 0.2.13 (engine 0.1.59), CUDA 13. A stage per platform, ending in `FROM strata-${TARGETARCH}`:
# BuildKit builds only the stages of the platform it targets. Update both platforms together, with
# the wrapper check in AGENTS.md.

# linux/amd64: upstream's published image (tag 0.2.13-cuda13), pinned by digest.
FROM ghcr.io/niko1221/strata@sha256:b6e1876515d5663047a7cd8dfee5223c8b2d117710eb2a9c285a433fb6e4a703 AS strata-amd64

# linux/arm64: upstream's release packages, at the hashes its signed linux-aarch64 feed lists, laid
# out as upstream's image does. The engine needs glibc 2.38 or newer.
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852 AS ubuntu

FROM ubuntu AS fetch-arm64
ADD --checksum=sha256:1579f7c6490e52334ca2d134b81534e6a5a351247a506918cbfd1f8285114523 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.13/strata-control-0.1.59-linux-aarch64.tar.gz /fetch/control.tar.gz
ADD --checksum=sha256:b213d9b173aae99645fc5922d42276fa1ed17ac67f7b788c55c912ba49e148f1 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.13/strata-engine-0.1.59-linux-aarch64-cuda-cuda13.tar.gz /fetch/engine.tar.gz
RUN set -e; \
    engine=/out/components/engine/0.1.59-linux-aarch64-cuda-cuda13; \
    mkdir -p /out/terminal "$engine"; \
    tar -xzf /fetch/control.tar.gz -C /out/terminal --no-same-owner; \
    tar -xzf /fetch/engine.tar.gz -C "$engine" --no-same-owner

FROM ubuntu AS strata-arm64
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends tini ca-certificates libgomp1 libatomic1 libnuma1 zlib1g \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/*
COPY --from=fetch-arm64 /out/ /opt/strata/
# Upstream's image environment: the server finds the engine package and keeps its state under /data.
ENV STRATA_COMPONENTS_DIR=/opt/strata/components \
    STRATA_INSTALL_DIR=/data/install \
    XDG_STATE_HOME=/data/state \
    XDG_CONFIG_HOME=/data/config \
    HOME=/data/home \
    PORT=8080 \
    LANG=C.UTF-8 \
    PYTHONUNBUFFERED=1 \
    PYTHONNOUSERSITE=1 \
    STRATA_IN_DOCKER=1 \
    NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility
VOLUME ["/data"]

FROM strata-${TARGETARCH}

# Upstream's entrypoint always turns an API key on. Dotcore runs its own keyless wrapper as the
# command instead, under the image's init.
COPY --chmod=0555 start.py /usr/local/bin/dotcore-strata-start

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["dotcore-strata-start", "--help"]
