# Installing Dotcore

## Requirements

- [Nix](https://nixos.org/download/). It builds Dotcore, its engines and its apps the same way on every platform, which is what lets one site mix machines. The [Determinate installer](https://determinate.systems/install/) is one way to get it.
- Linux (x86_64 or arm64):
  - [Docker Engine](https://docs.docker.com/engine/install/).
  - For NVIDIA GPUs, the [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html).
- macOS on Apple Silicon:
  - [Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/), with the default Docker socket allowed in its advanced settings.
  - Models run on the Mac's GPU directly. Docker is used for apps.
- OrbStack and Colima aren't supported yet.

## Install

The installer checks Docker and Nix, offers to install Nix when it's missing, and adds Dotcore to your Nix profile. While the repository is private, run it from a clone:

```sh
git clone https://github.com/dotcoreinc/dotcore-oss.git
/path/to/dotcore/installer/install.sh
```

- Run it by its path from outside the clone, such as from your home folder. Inside the clone, the development environment is loaded, and the installer stops.
- It installs from the clone's last commit. Commit your changes first: an install from uncommitted changes can't deploy models or apps.
- The first install builds Dotcore from source, which can take a while.
- `install.sh --help` lists its options.

This installs two commands:

- `dotcore` runs the console, the API and your models.
- `dotc` manages your site from the [command line](usage.md#command-line).

To install without the installer, add Dotcore to your Nix profile yourself, then start it:

```sh
nix profile add github:dotcoreinc/dotcore-oss
dotcore
```

## Update

Pull the clone, then run the installer again and choose to update. Your site is kept. A running `dotcore` keeps its old version until you restart it.

To start over instead, choose to reinstall: the installer removes Dotcore as the uninstaller does, then installs it again.

## First run

Run `dotcore` to start. The first run asks a few questions to set up your site, the group of machines Dotcore manages. One of them is what this machine runs. It can offer several:

- Models on NVIDIA GPUs, on Linux.
- Models on the GPU of a Mac.
- Apps in Docker.

Another asks for the address other machines reach this one at, usually its address on your local network. Dotcore listens only on that address. Then it prints the console's address: port 8080 on that address.

## Where files go

- Your site lives in `~/.local/state/dotcore/site`. It holds the site's settings, keys and app data.
- Downloads, such as model weights, go to `~/.cache/dotcore`. They can be downloaded again.
- `dotcore --site-dir` and `--cache-dir` change them.

## Uninstall

Stop `dotcore`, then run:

```sh
/path/to/dotcore/installer/install.sh uninstall
```

It asks before deleting your site or your downloads, and prints how to remove what it leaves, such as Nix and Docker images.

## Security

Dotcore has no login yet. Anyone who can reach the address you chose during setup can use your models and change your site, so choose one on a network you trust, such as your home or office network. See [SECURITY.md](../SECURITY.md).

Next: [Using Dotcore](usage.md).
