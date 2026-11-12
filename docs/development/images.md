# Images

Every Docker image the catalog uses is built by Nix. Each one has a folder under `apps/` or `engines/` with a `docker-image.nix` and a `Justfile`. The folder's README says what to know when using it, and its `AGENTS.md` how to update and test the image. The `add-app` skill in `.agents/skills/` says how to add one.

`just --list apps::<name>` or `just --list engines::<name>` shows an image's recipes. The common ones:

- `just image-load` loads the image into Docker.
- `just image-info` shows what it is pinned to.
- `just image-lock <repository>:<tag>` moves an upstream image to a new tag.
- `just image-store` builds a Dockerfile image with Docker on this machine, saves it into the Nix store (kept from garbage collection), and records its hash for this platform in `docker-image.stored.json`. Other machines then get that copy from a binary cache instead of building it.

## Upstream images

An upstream image is pinned in `docker-image.lock.json`, per platform. Change it only with `just image-lock`, never by hand.

## Dockerfile images

A Dockerfile image is built from a pinned source. `docker-image.stored.json` records the build of each platform, so other machines can copy it instead of building it again.

- After changing a Dockerfile image's inputs, run `just image-store` on an x86_64 Linux machine and on an arm64 one (a Mac works), and commit both records. A [remote host](remote-hosts.md) can run it for you.
- Until then, `just check` fails and loads fall back to a local build.
- Local builds can take long, such as ninfer's CUDA build, and give each machine different bytes.

## How a load works

A node loads an image when it prepares a deployment. It uses the first of these that works:

1. The image already in Docker.
2. The stored image, from the Nix store or a binary cache.
3. The upstream image, pulled by digest, or a local build of the Dockerfile.

Images load as `linux/amd64` on x86_64 Linux and `linux/arm64` on arm64 Linux and macOS.

## Nix artifacts from your checkout

In a development checkout, engines and apps are built from the checkout itself. A node only sees files that version control tracks, so run any `jj` command after adding a file.

## Nix artifacts from another repository

A site or development catalog may point a Nix artifact at a locked revision of any GitHub flake. The flake exposes the package as `legacyPackages.<system>.<package>`, an image package follows the loader conventions of [`nix/docker-images/`](../../nix/docker-images/) (a `docker-image-<name>` loader), and the catalog entry lists each system's store path, printed by `nix eval --raw <flake>#legacyPackages.<system>.<package>.outPath`.

## Disk use

- A stored image takes space twice, in the Nix store and in Docker. CUDA images take 10 to 20 GB each.
- `dotc gc` removes image names nothing uses anymore. An image you loaded by hand with `just image-load` counts as unused.
- `just image-store` keeps its image under `~/.local/state/dotcore/docker-images` until you remove it.
- `docker buildx prune` clears Docker's build cache.

## Troubleshooting

- If a load says the image doesn't match its platform or identity, remove it with `docker image rm <reference>`, using the reference from `just image-info`, then load again.
- If a new `docker-image.nix` or lock file is ignored, run any `jj` command so it gets tracked.
- If a load can't reach Docker, check `DOCKER_HOST` and `docker context ls`.
