# ComfyUI agentic instructions

## How it is built

* [`Dockerfile`](Dockerfile) builds the upstream source at `COMFYUI_REV` in
  [`docker-image.nix`](docker-image.nix). [`entrypoint.sh`](entrypoint.sh) prepares the deployment's
  storage, which mounts on ComfyUI's base directory. The shared models storage mounts on the base
  directory's models folder.
* The download node in [`custom_nodes/dotcore_model_downloads`](custom_nodes/dotcore_model_downloads)
  decides which missing-model downloads are allowed. Read it before changing download behavior. It
  ships in the image, outside the storage, so each image version brings its own copy.
* GPUs reach the container only through Dotcore's device requests (see the `Dockerfile`). A device
  CUDA 13 can't use stops the container instead of falling back to the CPU.

## Update

1. Set the new upstream commit as `COMFYUI_REV` in [`docker-image.nix`](docker-image.nix).
2. For a new torch or CUDA base, edit the base image digest or the torch pins in the
   [`Dockerfile`](Dockerfile). The other Python packages follow the source's own
   `requirements.txt`, unpinned like the apt packages: the stored image fixes what deployments run.
3. When `just image-test` reports that the frontend no longer hands missing-model downloads to the
   bridge, or allows other sources or suffixes, follow the new frontend in the download node.
4. Changing the source commit, [`Dockerfile`](Dockerfile), [`entrypoint.sh`](entrypoint.sh),
   [`extra-model-paths.yaml`](extra-model-paths.yaml) or the bundled custom nodes makes the build
   records in `docker-image.stored.json` stale: run `just image-store` on a `linux/amd64` host and
   on a `linux/arm64` host, then commit both entries; see
   [Dockerfile images](../../docs/development/images.md#dockerfile-images) for running it remotely.
5. Run the checks below, then set `version` in the catalog declaration.

## Validate

1. `just image-test`, or every application image workflow from the root with `just apps::test`:
   against the built image, a CPU-only start as root serving `/system_stats` from a storage-only
   mount, torch's CUDA 13 kernels, the model download node loading from the image, downloading a
   small public Hugging Face file into its model folders and refusing unsafe requests, and the
   frontend still handing the dialog's downloads to it.
2. Gated, on an NVIDIA host: give the Docker application execution target the accelerator device,
   deploy ComfyUI, and open the endpoint the console reports.
3. Place a small checkpoint in the shared models storage, then queue one workflow from the editor.
4. The queue completes the workflow, `/system_stats` reports the accelerator, and the deployment's
   inspection attributes accelerator memory to its service. The placed checkpoint and the
   generated image are still on storage after a restart, and the placed checkpoint is still there
   after removing the deployment and deploying ComfyUI again.
