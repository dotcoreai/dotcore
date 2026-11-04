# ComfyUI v0.42.5, built here from the upstream source commit docker-image.nix
# passes in, on NVIDIA's CUDA 13 runtime image. The cu130 PyTorch wheels
# below carry the kernels ComfyUI enables for RTX 20-series and newer
# accelerators; earlier CUDA lines leave them off.
FROM nvidia/cuda:13.0.1-runtime-ubuntu24.04@sha256:c3fde347d52d578c84fd644bc177bc7ec333feaf11550d990da4084d7612e4c7

# Accelerators arrive only through the CDI devices Dotcore requests, never through this base
# image's default of exposing every device to a daemon that registers the legacy NVIDIA runtime.
ENV DEBIAN_FRONTEND=noninteractive \
    NVIDIA_VISIBLE_DEVICES=void \
    PATH=/opt/comfyui/venv/bin:$PATH \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    PYTHONUNBUFFERED=1

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates python3 python3-venv \
 && rm -rf /var/lib/apt/lists/*

# Torch installs before the source is fetched, so a source bump does not download it again. The
# cu130 index carries the Blackwell (sm_120) kernels ComfyUI requires.
RUN python3 -m venv /opt/comfyui/venv \
 && pip install --index-url https://download.pytorch.org/whl/cu130 \
      torch==2.11.0+cu130 torchvision==0.26.0+cu130

ARG COMFYUI_REV
ADD https://github.com/comfyanonymous/ComfyUI.git#${COMFYUI_REV} /opt/comfyui/app
RUN pip install -r /opt/comfyui/app/requirements.txt

# Custom nodes the image ships, such as the server-side model download node, live outside the
# storage attachment so each image version brings its own; the entrypoint passes the paths file.
COPY extra-model-paths.yaml /opt/comfyui/extra-model-paths.yaml
COPY custom_nodes /opt/comfyui/custom_nodes
COPY --chmod=0555 entrypoint.sh /opt/comfyui/entrypoint.sh

WORKDIR /opt/comfyui/app
EXPOSE 8188
ENTRYPOINT ["/opt/comfyui/entrypoint.sh"]
