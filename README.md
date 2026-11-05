# Dotcore

**Local AI, without the setup.**

Dotcore runs AI on hardware you own. It loads models, fits them to your machines, and puts a single endpoint in front of everything.

- **Pick a model, get a running endpoint.** Dotcore starts it for you, on llama.cpp, vLLM or ninfer.
- **Fit the model to your hardware.** See how much memory a model needs and the largest context that fits each machine.
- **One endpoint in front of everything.** Call every model through one OpenAI-compatible API. If a deployment is down, requests move to another one.
- **Many machines, working as one.** Start on one machine, then add more.
- **Apps next to your models.** Run apps on the same machines, for example Open WebUI for chat or ComfyUI for images.

> [!IMPORTANT]
> Dotcore is in alpha. Expect breaking changes, including to saved sites. Security is best effort for now and not guaranteed. See [SECURITY.md](SECURITY.md).

## Quick start

It runs on Linux (x86_64, arm64) and macOS (Apple Silicon). You need:

- [Nix](https://nixos.org/download/). It builds Dotcore and every engine the same way on each system, which is what lets one site mix machines and engines.
- [Docker Engine](https://docs.docker.com/engine/install/) on Linux, or [Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/) on macOS.

```sh
git clone https://github.com/dotcoreai/dotcore.git
/path/to/dotcore/installer/install.sh
```

Follow the installer's instructions. See [docs/install.md](docs/install.md) for the full steps.

## Documentation

- [Install and set up](docs/install.md)
- [Using Dotcore](docs/usage.md)
- [Developing Dotcore](docs/development/README.md)
- [Architecture](docs/architecture/architecture.md)
- [Product specification](docs/product/product-spec.md)

## Contributing

Questions and ideas go to [Discussions](https://github.com/dotcoreai/dotcore/discussions). To send a change, read [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Report a vulnerability privately, as described in [SECURITY.md](SECURITY.md).

## License

Copyright 2026 Dotcore Inc. and contributors. Licensed under Apache-2.0, see [LICENSE](LICENSE).

The license covers the code, not the Dotcore name or logo:

- Projects built for Dotcore may say "for Dotcore" or "compatible with Dotcore".
- Modified versions you distribute must use a different name and logo.
- The Dotcore logo is for the official software and website only.
