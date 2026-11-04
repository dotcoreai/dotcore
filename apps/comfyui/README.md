# ComfyUI application

[ComfyUI](https://github.com/Comfy-Org/ComfyUI), a node-based editor for generating images and
video.

## Using it

* Open it from the console.
* When a workflow needs a model you don't have, the editor offers to download it onto the server.
  Public models from Hugging Face and Civitai work. Models that need a login don't; download them
  yourself and place them in the shared models storage.
* Downloaded models live in storage shared by every ComfyUI deployment on the node. They survive
  restarts and removing a deployment, and stay until the site is reset or an operator removes them.
* Custom nodes, inputs, outputs and user state live in the deployment's own storage. Removing the
  deployment deletes them.

## Good to know

* It has no login, and custom nodes can run any code. Use it on a trusted network only.
* Deployments on one node share their models, so one deployment can place a model file, including
  formats that run code when loaded, that another loads. Each already runs as root without a login,
  so this grants no new authority.
* It uses an NVIDIA GPU when the machine has one (driver 580 or newer), and the CPU otherwise, as
  on a Mac.
* Keep the VRAM mode setting on `auto`. The other modes need an NVIDIA GPU.
* Custom nodes that need extra Python packages aren't supported.
* Dotcore doesn't estimate its memory use, since it depends on each workflow.

Maintaining it: [`AGENTS.md`](AGENTS.md).
