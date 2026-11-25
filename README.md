# Dotcore

**Local AI, without the setup.**

Dotcore runs AI on hardware you own. It loads models, fits them to your machines, and puts a single endpoint in front of everything.

- **Pick a model, get a running endpoint.** Dotcore starts it for you, on llama.cpp, vLLM, etc.
- **Fit the model to your hardware.** See how much memory a model needs and the largest context that fits each machine.
- **One endpoint in front of everything.** Call every model through one OpenAI-compatible API. If a deployment is down, requests move to another one.
- **Many machines, working as one.** Start on one machine, then add more.
- **Apps next to your models.** Run apps on the same machines, for example Open WebUI for chat or ComfyUI for images.

> [!IMPORTANT]
> Dotcore is in alpha. Expect breaking changes, including to saved sites. Security is best effort for now. See [SECURITY.md](SECURITY.md).

## Quick start

Dotcore runs on Linux (x86_64, arm64) and macOS (Apple Silicon). Models run on NVIDIA GPUs on Linux, or on the GPU of an Apple Silicon Mac. Other accelerators, starting with AMD, are on the [roadmap](#roadmap).

You'll need:

- [Nix](https://nixos.org/download/), which builds Dotcore and the engines on your machine.
- Docker:
  - Linux: [Docker Engine](https://docs.docker.com/engine/install/), plus the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) for NVIDIA GPUs.
  - macOS: [Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/).

Run this in a terminal and follow the installer's instructions:

```sh
curl -fsSL https://dotcore.ai/install.sh | sh
```

See [docs/install.md](docs/install.md) for the full steps, then [Run your first model](docs/usage.md#run-your-first-model).

## Roadmap

- **Authentication** Sign in and control who can reach your site.
- **Share models with friends** Use a friend's models, and let them use yours, without setting anything up by hand.
- **Better sizing from real use** Opt in to share how models perform and how much memory they use, so everyone gets better estimates.
- **More accelerators** AMD first, then others such as Intel.
- **Binary cache** Prebuilt downloads, so installs don't build everything on your machine.
- **Windows support**
- **More models, engines and apps** Grow what you can run.

## Documentation

- [Install and set up](docs/install.md)
- [Using Dotcore](docs/usage.md)
- [Developing Dotcore](docs/development/README.md)
- [Product and architecture](docs/specs/README.md)

## Contributing

Questions, ideas and bug reports go to [Discussions](https://github.com/dotcoreai/dotcore/discussions). Pull requests fix an accepted issue. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Report a vulnerability privately, as described in [SECURITY.md](SECURITY.md).

## License

Copyright 2026 Dotcore Inc. and contributors. Licensed under Apache-2.0, see [LICENSE](LICENSE).
