#!/bin/sh
set -eu

# ComfyUI reads custom nodes and checkpoints from the base directory but creates neither folder,
# so an empty storage volume needs them before startup.
mkdir -p /data/custom_nodes /data/models/checkpoints

# A container without an NVIDIA device node runs CPU-only, as on Mac hosts and deviceless boxes:
# ComfyUI needs the flag to fall back instead of failing on the missing driver. A device node that
# CUDA cannot use is a broken GPU deployment, so it stops instead of going Ready on the CPU. The
# probe runs isolated from the working directory so ComfyUI's source tree cannot shadow torch.
cpu_flag=
if [ -e /dev/nvidiactl ]; then
  if ! (cd / && python -I -c 'import sys, torch; sys.exit(0 if torch.cuda.is_available() else 1)'); then
    echo "entrypoint: an NVIDIA device is present but CUDA is unavailable. This image needs CUDA 13, so the host NVIDIA driver must be 580 or newer, and the device must come from a current CDI specification (nvidia-ctk cdi generate)." >&2
    exit 70
  fi
  echo "entrypoint: NVIDIA device present and CUDA available, running on the accelerator"
else
  cpu_flag=--cpu
  echo "entrypoint: no NVIDIA device present, running CPU-only"
fi

# The VRAM mode deployment parameter selects one of ComfyUI's mutually exclusive memory modes;
# auto passes no flag and keeps ComfyUI's default dynamic VRAM. Those flags also exclude --cpu.
case "${COMFYUI_VRAM_MODE:-auto}" in
  auto) vram_flag= ;;
  highvram) vram_flag=--highvram ;;
  gpu-only) vram_flag=--gpu-only ;;
  novram) vram_flag=--novram ;;
  *)
    echo "entrypoint: unknown VRAM mode ${COMFYUI_VRAM_MODE}; expected auto, highvram, gpu-only, or novram." >&2
    exit 64
    ;;
esac
if [ -n "$cpu_flag" ] && [ -n "$vram_flag" ]; then
  echo "entrypoint: VRAM mode ${COMFYUI_VRAM_MODE} needs a GPU, but this container has no NVIDIA device. Redeploy with VRAM mode auto, or on a target that passes a GPU." >&2
  exit 64
fi

# The storage attachment mounts on the base directory, so checkpoints, custom nodes, inputs,
# generated images, and user state all live on storage. The extra paths add the image's own
# custom nodes after those on storage.
exec python /opt/comfyui/app/main.py \
  --base-directory /data \
  --listen 0.0.0.0 \
  --port 8188 \
  --disable-auto-launch \
  --extra-model-paths-config /opt/comfyui/extra-model-paths.yaml \
  $cpu_flag \
  $vram_flag \
  "$@"
