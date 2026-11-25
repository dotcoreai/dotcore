# Strata 0.2.21 (engine 0.1.67), CUDA 13. Control is 0.1.67 on amd64 (the image's) and 0.1.68 on
# arm64, whose signed feed lists no 0.1.67 package; the wrapper uses nothing 0.1.68 changed. A
# stage per platform, ending in `FROM strata-${TARGETARCH}`: BuildKit builds only the stages of the
# platform it targets. Update both platforms together, with the wrapper check in AGENTS.md.

# linux/amd64: upstream's published image (tag 0.2.21-cuda13), pinned by digest.
FROM ghcr.io/niko1221/strata@sha256:992c38a2c6dd42d5cf23cabf2f4ba7702abef0cc1b82cfeb17239da822e081b4 AS strata-amd64

# linux/arm64: upstream's release packages, at the hashes its signed linux-aarch64 feed lists, laid
# out as upstream's image does. The engine needs glibc 2.38 or newer.
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852 AS ubuntu

FROM ubuntu AS fetch-arm64
ADD --checksum=sha256:d9c0cb98798ed3bb14c1bb02e5e4d7235cbc444bf977e9410d32c9f442409958 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.21/strata-control-0.1.68-linux-aarch64.tar.gz /fetch/control.tar.gz
ADD --checksum=sha256:1fc5967920102934190678f11817b91121f82c41c4d96312353c04ccc5e8531a \
    https://github.com/Niko1221/Strata/releases/download/v0.2.21/strata-engine-0.1.67-linux-aarch64-cuda-cuda13.tar.gz /fetch/engine.tar.gz
ADD --checksum=sha256:58ec7bf4af0db8a3026f54be0dca3aa6be4e22bae607f773568af7493741f368 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.21/strata-inkling-0.1.67-linux-aarch64-cuda-cuda13.tar.gz /fetch/inkling.tar.gz
RUN set -e; \
    engine=/out/components/engine/0.1.67-linux-aarch64-cuda-cuda13; \
    inkling=/out/components/shapes/inkling2/engine/0.1.67-linux-aarch64-cuda-cuda13; \
    mkdir -p /out/terminal "$engine" "$inkling"; \
    tar -xzf /fetch/control.tar.gz -C /out/terminal --no-same-owner; \
    tar -xzf /fetch/engine.tar.gz -C "$engine" --no-same-owner; \
    tar -xzf /fetch/inkling.tar.gz -C "$inkling" --no-same-owner

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
