# Hermes agentic instructions

## How it is built

* [`Dockerfile`](Dockerfile) wraps the upstream image pinned by digest. The node loads the stored
  image when it has one, otherwise builds the `Dockerfile` on the first deployment.
* [`start.sh`](start.sh) writes Hermes' configuration from the catalog's values and keeps the rest of
  its data. Read it for what it checks and sets. Its placeholder key `dotcore-no-key` is not a
  secret: Hermes lists models only for a provider that has a key.
* The dashboard refuses a non-loopback bind without a login, and the Chat page's WebSockets work
  only behind it. The catalog sets Hermes' `basic` login through literal bindings. Its username and
  password are deliberate public placeholders, shown in the catalog `notes`, the same for every
  deployment; they are not a leaked secret. They set no session secret, so a restart logs users out.
  Its test reaches the login and chat through a published port, never from inside the container,
  which sees loopback.

## Update

1. Pick an upstream release and read its index digest:
   `docker buildx imagetools inspect nousresearch/hermes-agent:<tag>`.
2. Replace the digest on the `FROM` line of [`Dockerfile`](Dockerfile), then `just image-test`.
3. Changing `Dockerfile` or `start.sh` makes the store build records in `docker-image.stored.json`
   stale: run `just image-store` on a `linux/amd64` host and on a `linux/arm64` host (a Mac works;
   see [Dockerfile images](../../docs/development/images.md#dockerfile-images) for running it
   remotely). Each run updates only its platform's entry; commit both.
4. Bump `version` in the catalog declaration.

## Validate

1. `just image-test`: against the image tagged `dotcore/hermes:dev`, the startup wrapper's
   configuration and refusals, gateway readiness, the Dotcore provider selection, and the login and
   Chat page WebSocket through a published port, as a browser reaches them.
2. Deploy Hermes with a linked inference route and open the dashboard after readiness, logging in
   with the login from the console's app panel: Dotcore is the selected provider and the selectable
   model is the route's current name.
3. Open the dashboard's Chat page and send one prompt; the backend log shows
   `POST /v1/chat/completions` with that route name as `model_id`. Rename the route and check the
   selection follows. Without the dashboard:
   `docker exec <gateway container> hermes chat -Q --max-turns 1 -q "Reply with exactly: ok"`.
4. Hermes first probes `/api/tags`, `/v1/props`, `/version` and others; their 404s are expected.
