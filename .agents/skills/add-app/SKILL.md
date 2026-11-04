---
name: add-app
description: Add a new application to the Dotcore master catalog, with the Docker image it runs, from the image folder to a deployed and tested app. Use when asked to add, port or package an app, or to add a new app or engine image.
---

# Add an app

## Context

An application is one YAML file in `catalog/applications/`: a graph of services, each running a Docker image that Nix builds, with its ports, configuration and storage bindings, readiness check and primary endpoint. The neighbouring files are the reference: `n8n.yaml` runs an unmodified upstream image, `hermes.yaml` an image built from a Dockerfile. Their comments show how a service reaches the inference endpoint and other values the site resolves.

Each image has a folder under `apps/` (or `engines/` for an engine image) with a `docker-image.nix`, a `Justfile`, a `README.md` and an `AGENTS.md`. `apps/AGENTS.md` owns the image rules and the folder doc templates, `nix/AGENTS.md` the image derivation rules, and `docs/development/images.md` explains how images are locked, stored and loaded. Platform coverage follows the root "Platform scope" rule unless the user names a narrower target.

## Instructions

1. Gather the facts from the upstream project: the image and its tags, supported platforms, ports, data folders, health endpoint, how it is configured, how it reaches an OpenAI-compatible endpoint, its login, and its license.
   * Prefer the unmodified upstream image. Build from a Dockerfile only when the app needs changes the catalog graph cannot make (a start wrapper, missing tools).

2. Create the image folder `apps/<name>/` (`engines/<name>/` and `engines/Justfile` for an engine image); image names are unique across both.
   * Copy the `Justfile` from a folder of the same kind (`n8n` for upstream images, `hermes` for Dockerfile images); the recipes take the image name from the folder.
   * Write `docker-image.nix` like that folder's.
   * In `apps/Justfile`, add the module, and add the folder to the `image-load` aggregate, and to `test` when it has an `image-test` recipe.
   * List the image in the `appImages` set of `flake.nix` (`engineImages` for an engine), then run any `jj` command so the flake sees the new files.

3. Pin the image, from the folder.
   * Upstream image: `just image-lock <repository>:<tag> linux/amd64 linux/arm64`. Every image package fails to evaluate until this lock exists.
   * Dockerfile image: `just image-store` on a host of each platform (remote hosts follow `dev/AGENTS.md`).

4. Write `catalog/applications/<name>.yaml` from the closest neighbour.
   * Reference the package `apps.<name>.docker` with `dockerImage: {}`, and list the platforms the image was pinned for.
   * Bind the inference endpoint and published URLs with resolved values rather than literals, and keep any fixed login visible in `notes`.
   * Leave out optional fields you cannot support.

5. Write the folder's `README.md` and `AGENTS.md` from the templates in `apps/AGENTS.md`.

6. Validate.
   * `just image-load` in the folder, then `just image-test` when the image has one.
   * `just backend::test` loads the whole catalog.
   * Deploy the app on a development site that runs apps on Docker (`dev/AGENTS.md`), open its endpoint, and check that it reaches a deployed model.
   * Stop and ask the user if the image fails to start or cannot reach the inference endpoint; do not improvise workarounds.

7. Record in the change description the image source and version, the platforms pinned, and what each validation showed.
