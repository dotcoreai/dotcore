# App and engine image agentic instructions

These rules cover every image folder, `apps/<app>/` and `engines/<engine>/`. Each folder's own
`AGENTS.md` adds how to update and validate that image. [`engines/AGENTS.md`](../engines/AGENTS.md)
says where engine rules live, and [`nix/AGENTS.md`](../nix/AGENTS.md) owns the image derivation rules
(locks, store records, loaders). `just --list` in a folder shows its recipes, and
[Images](../docs/development/images.md) explains how loads work.

Use the `add-app` skill to add an app or engine image.

## Keep images small

* Add only what the app or engine needs to run, per "Code" in the root [`AGENTS.md`](../AGENTS.md).
* Don't pin what the stored image already fixes: a rebuild where the stored image is unavailable is
  best effort ([Dockerfile images](../docs/development/images.md#dockerfile-images)).

## Folder docs

Every image folder has a `README.md` for the people who use the app or engine, and an `AGENTS.md`
for whoever maintains the image:

```markdown
# <Name> application            (README.md; or: engine)

<One sentence: what it is for, linking the upstream project.>

## Using it
* Apps only: how to open it and how it reaches the site's models, in a few bullets.

## <Feature>
* Optional: a feature only some users need, such as n8n's webhooks.

## Good to know
* What a user can trip on: login, where data lives, hardware, known limits.

Maintaining it: [`AGENTS.md`](AGENTS.md).
```

```markdown
# <Name> agentic instructions   (AGENTS.md)

## How it is built
* Optional: pointers to the files that own the build and its behavior, plus gotchas the code
  doesn't make obvious. Point, don't describe.

## Update
1. Steps to move to a new upstream version, ending with the catalog change.

## Validate
1. Automated checks first (`just image-test`, `just backend::test-nvidia`, ...), then manual ones.
```

* READMEs are for users: no image sources, catalog paths, build or update detail. Those go in
  `AGENTS.md`.
* Bullets and numbered steps only; no other sections. Recipes stay in the `Justfile`, listed by
  `just --list`. Detail the code owns stays in the code.
