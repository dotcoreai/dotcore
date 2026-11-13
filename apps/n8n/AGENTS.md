# n8n agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image; nothing is built here.
* The catalog declaration owns the start command, which imports the `Dotcore Inference` credential,
  n8n's settings and the owner login (email and password parameters; the password is bound in bcrypt form because n8n accepts only a hash). Read it before changing them. The command stays on one
  line because the Docker runtime rejects control characters.
* Webhook and editor URLs are fixed from the node's host and port when the deployment is created.
* A deployment keeps the declaration `notes` it was created with.

## Update

1. Pick a release tag (see `stable` on Docker Hub):
   `just image-lock docker.io/n8nio/n8n:<version>` (locks linux/amd64 and linux/arm64).
2. Run the checks below, then set `version` in the catalog declaration.

## Validate

1. On a Docker apps site, deploy a model and n8n, open the endpoint the console shows, and sign in with the sidebar login; a second deployment gets a different generated password and `admin` is refused.
2. Credentials lists one `Dotcore Inference`; an OpenAI Chat Model node using it lists the site's
   routes, and one execution reaches a route (backend log `POST /v1/chat/completions` with that
   route as `model_id`).
3. An active workflow's webhook at the published endpoint answers.
4. After a container restart and a backend restart, sign-in, the single credential, workflows and
   the port are unchanged.
