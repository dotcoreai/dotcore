# Strata 0.2.13 (engine 0.1.59), the CUDA 13 build for linux/amd64 (tag 0.2.13-cuda13), pinned by
# digest. Update it only together with the wrapper check in AGENTS.md.
FROM ghcr.io/niko1221/strata@sha256:b6e1876515d5663047a7cd8dfee5223c8b2d117710eb2a9c285a433fb6e4a703

# Upstream's entrypoint always turns an API key on. Dotcore runs its own keyless wrapper as the
# command instead, under the image's init.
COPY --chmod=0555 start.py /usr/local/bin/dotcore-strata-start

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["dotcore-strata-start", "--help"]
