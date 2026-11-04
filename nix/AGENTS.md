# Nix agentic instructions

Nix builds everything Dotcore ships: the `dotcore` package, each engine's native package, every app
and engine Docker image, and the dev shell. `flake.nix` at the root wires the outputs; this folder
holds the build code. These rules cover what agents get wrong.

## Layout

- `all-in-one/`: the `dotcore` package (launcher, composition of the backend, console and ingress,
  master catalog build).
- `backend.nix`, `frontend.nix`: each component's build; keep component code out of `all-in-one/`.
- `ingress.nix`: the Caddy web server, shared by the package and the development stack.
- `engines.nix`: builds each `engines/<name>/package.nix` on the systems its `meta.platforms`
  lists, with the root nixpkgs. Engines are flake packages, not separate flakes.
- `docker-images/`: the Docker image library that builds each folder's `docker-image.nix`.

## Using the flake

- The flake sees only files version control knows about: after creating a file a build needs, run
  any `jj` command so it is recorded.
- Refer to outputs as `.#<output>` from the repository root, never `path:.`, which copies
  `target/` and `node_modules/` into the store.

## Flake outputs

- `flake.nix` lists each folder's `package.nix` and `docker-image.nix` and places them under
  `legacyPackages.<system>`: `engines.<name>.native`, `engines.<name>.docker`, `apps.<name>.docker`,
  and tooling under `tools.<name>`. `packages` holds only products. `nix flake show --legacy` shows
  the tree.
- Catalog entries name packages by that path, so moving or renaming a folder changes the catalog's
  `package` values.
- A package exists only on the systems it supports.
- A second build of the same kind takes a sibling name, such as `docker-rocm`: a leaf can't gain
  children.
- Builds never read flake source metadata (`self.rev`, `lastModified`): every node builds a locked
  revision, from local source or GitHub, and must get the same store paths.
- Don't add a standard `apps` output: `nix run` looks there first, so it would shadow the `apps`
  tree.

## Docker images

- Each image folder owns its recipes and its `AGENTS.md`. Use the `add-app` skill to add an image;
  [Images](../docs/development/images.md) explains loads, locks, store records and troubleshooting.
- Load, build and store catalog images only through their packages; recipes never run
  `docker build` or `docker pull`.
- Change a registry lock only with that folder's `image-lock` recipe, never by hand.
- After changing a Dockerfile image's inputs, run its `image-store` recipe on each platform;
  `checks.<system>.docker-images-records` fails until then.
- Every master catalog image is a Docker-delivered Nix artifact, never an `oci` reference. `oci`
  stays in the contract for catalogs people write themselves.
- The backend finds an image's loader and its launch reference from the store entry name
  (`docker-image-<name>`, repeated in `meta.mainProgram`). Change that name or the tag rule only
  together with `dotcore-domain` and the loader tests.
- Test loader changes in `docker-images/tests`, and run `just dev::docker-images-smoke` on Linux
  and on a Mac.
- The backend reads the image metadata and the loader's result line. After changing their fields,
  update the backend's reader and run `just dev::docker-images-samples`.

## Master catalog

- The packaged master catalog points its Nix artifacts at `github:dotcoreinc/dotcore/<rev>`, with
  store paths for every system, evaluated but not built. `just dev::catalog-materialize` builds it.
- Built from an uncommitted tree, it has no revision: artifacts keep pointing at the local flake
  and evaluation warns.
- `all-in-one/catalog.nix` and `materialize-catalog.jq` own the rules that make a catalog
  release match its package (version, executable, loader name, platform, kind).

## Checks

- `just dev::docker-images-check` runs the image checks. Other flake checks:
  - `dotcore-tests`: the packaged runtime.
  - `dotcore-failures`: the backend and Caddy exiting under the launcher.
  - `dotcore-catalog`: the master catalog build at a fixed revision.
  - `legacy-packages-eval`: evaluates every `legacyPackages` leaf on every system, which
    `nix flake check` skips, and catches builds that read source metadata.
- No check covers Caddy forwarding SSE and gRPC-Web: test it by hand after changing the ingress.

## Environment

- Never wire `LD_LIBRARY_PATH` or `opengl-driver` paths into the wrapper, the dev shell or test
  recipes. The code finds vendor libraries itself (`backend/nvml`, see
  [`backend/AGENTS.md`](../backend/AGENTS.md)).
- `SSL_CERT_FILE` is the one deliberate default: tests point it at the Nix CA bundle for TLS
  clients in the sandbox, and the installed `dotc` sets it only when the caller hasn't.
- Every Dotcore host needs Nix: a node builds the Nix artifacts it runs for its own system.
