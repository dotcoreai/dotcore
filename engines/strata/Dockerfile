# Strata 0.2.19 (engine 0.1.65), CUDA 13. A stage per platform, ending in `FROM strata-${TARGETARCH}`:
# BuildKit builds only the stages of the platform it targets. Update both platforms together, with
# the wrapper check in AGENTS.md.

# linux/amd64: upstream's published image (tag 0.2.19-cuda13), pinned by digest.
FROM ghcr.io/niko1221/strata@sha256:5cf423c2ad9d2f626da250ade74cb9b48087279bff636fca3c7c08eb4f786d26 AS strata-amd64

# linux/arm64: upstream's release packages, at the hashes its signed linux-aarch64 feed lists, laid
# out as upstream's image does. The engine needs glibc 2.38 or newer.
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852 AS ubuntu

FROM ubuntu AS fetch-arm64
ADD --checksum=sha256:177269e2164221d43314bcb2df45aa33dc23b0902f544e2cc0af0775cf060de3 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.19/strata-control-0.1.65-linux-aarch64.tar.gz /fetch/control.tar.gz
ADD --checksum=sha256:bdc67b8850c85bff240dab24faeddd2877275a43de5b47cd07e53fb41bd9bc27 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.19/strata-engine-0.1.65-linux-aarch64-cuda-cuda13.tar.gz /fetch/engine.tar.gz
ADD --checksum=sha256:2a89c1ded0ceefccff7cf2130fd35597869d0113d72cb3fd27bc2a9006fa03cc \
    https://github.com/Niko1221/Strata/releases/download/v0.2.19/strata-inkling-0.1.65-linux-aarch64-cuda-cuda13.tar.gz /fetch/inkling.tar.gz
RUN set -e; \
    engine=/out/components/engine/0.1.65-linux-aarch64-cuda-cuda13; \
    inkling=/out/components/shapes/inkling2/engine/0.1.65-linux-aarch64-cuda-cuda13; \
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
