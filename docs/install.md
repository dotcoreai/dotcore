# Installing Dotcore

## Requirements

- [Nix](https://nixos.org/download/). It builds Dotcore, its engines and its apps. The installer offers to install Nix if it's missing.
- Linux (x86_64 or arm64):
  - [Docker Engine](https://docs.docker.com/engine/install/).
  - For NVIDIA GPUs, the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html).
- macOS on Apple Silicon:
  - [Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/), with the default Docker socket allowed in its advanced settings.
  - Models run on the Mac's GPU directly. Docker is used for apps.
- OrbStack and Colima aren't supported yet.

## Install

In an interactive terminal, run the installer from the Dotcore site. It checks Docker and Nix, then adds Dotcore to your Nix profile:

```sh
curl -fsSL https://dotcore.ai/install.sh | sh
```

There's no binary cache yet, so the first install takes longer. Nix builds Dotcore on your machine.

This installs two commands:

- `dotcore` runs the console, the API and your models.
- `dotc` manages your site from the [command line](usage.md#command-line).

To install without the installer, enable flakes in Nix (`experimental-features = nix-command flakes` in `~/.config/nix/nix.conf`), then add Dotcore to your Nix profile yourself and start it. Before Nix 2.30, use `nix profile install`:

```sh
nix profile add github:dotcoreai/dotcore
dotcore
```

## First run

Run `dotcore` to start. The first run asks a few questions to set up your site, the group of machines Dotcore manages. One of them is what this machine runs. It can offer several:

- Models on NVIDIA GPUs, on Linux.
- Models on the GPU of a Mac.
- Apps in Docker.

Another asks for the address other machines reach this one at, usually its address on your local network. Dotcore listens only on that address. Then it prints the console's address: port 8080 on that address.

## Add this machine to a site

To add a new machine to a site that already runs, start from the manager: **Machines**, then **Add machine** in the console, or `dotc node add`. It shows a command with a join token for the new machine:

```sh
curl -fsSL https://dotcore.ai/install.sh | sh -s -- --join <token>
```

It installs Dotcore, joins the site once you approve the machine on the manager, then starts Dotcore. Keep the console's dialog or `dotc node add` open until the machine joins. The token expires after a few minutes, which a first install can outlast; if it does, get a new command and run its `dotc node join` form on the machine. See [adding machines](usage.md#add-more-machines).

## Security

Dotcore has no login yet. Anyone who can reach the address you chose during setup can use your models and change your site, so choose one on a network you trust, such as your home or office network. See [SECURITY.md](../SECURITY.md).

## Update

> [!WARNING]
> Dotcore is in alpha. Expect breaking changes in updates, including to saved sites, until the first stable release.

Run the [install command](#install) again and choose to update. The update keeps your site's files. If `dotcore` is running, restart it to use the new version.

To start over instead, choose to reinstall: the installer removes Dotcore as the uninstaller does, then installs it again.

## Data folders

- Your site lives in `~/.local/state/dotcore/site`. It holds the site's settings, keys and app data.
- Downloads, such as model weights, go to `~/.cache/dotcore`. They can be downloaded again.
- `dotcore --site-dir` and `--cache-dir` change them.

## Uninstall

Stop `dotcore`, then run:

```sh
curl -fsSL https://dotcore.ai/install.sh | sh -s -- uninstall
```

It asks before deleting your site or your downloads, and prints how to remove what it leaves, such as Nix and Docker images.

---

Next: [Using Dotcore](usage.md).
