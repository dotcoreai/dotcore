# Strata 0.2.23 (engine 0.1.69, control and Inkling shape engine 0.1.70), CUDA 13. The engine
# tarball is the one on the v0.2.22 release, which v0.2.23's signed feed still lists. A stage per
# platform, ending in `FROM strata-${TARGETARCH}`: BuildKit builds only the stages of the platform
# it targets. Update both platforms together, with the wrapper check in AGENTS.md.

# linux/amd64: upstream's published image (tag 0.2.23-cuda13), pinned by digest.
FROM ghcr.io/niko1221/strata@sha256:547f07819a53675a7b04f11a9440ad96d75a3218638a83ca0e508ccb293db7d1 AS strata-amd64

# linux/arm64: upstream's release packages, at the hashes its signed linux-aarch64 feed lists, laid
# out as upstream's image does. The engine needs glibc 2.38 or newer.
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852 AS ubuntu

FROM ubuntu AS fetch-arm64
ADD --checksum=sha256:c28ee3f1cc769c4e5cbca1a6038af31a653fa087e05e22ab8ed970dcf36af1e1 \
    https://github.com/Niko1221/Strata/releases/download/v0.2.23/strata-control-0.1.70-linux-aarch64.tar.gz /fetch/control.tar.gz
ADD --checksum=sha256:07943058ee0cebe1c938a99e7b7e181c108fe93b54ef9f5eb14effaa4b461cab \
    https://github.com/Niko1221/Strata/releases/download/v0.2.22/strata-engine-0.1.69-linux-aarch64-cuda-cuda13.tar.gz /fetch/engine.tar.gz
ADD --checksum=sha256:a3382961244194629780515199670e510953bcd73d06576b0844af510ed6d47f \
    https://github.com/Niko1221/Strata/releases/download/v0.2.23/strata-inkling-0.1.70-linux-aarch64-cuda-cuda13.tar.gz /fetch/inkling.tar.gz
RUN set -e; \
    engine=/out/components/engine/0.1.69-linux-aarch64-cuda-cuda13; \
    inkling=/out/components/shapes/inkling2/engine/0.1.70-linux-aarch64-cuda-cuda13; \
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
