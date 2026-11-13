# Open WebUI agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image; nothing is built here.
* Login is on and sign-up is off. The admin email and password are parameters; Open WebUI creates
  that admin only when its data folder has no user, so login needs a fresh data folder, which each
  deployment has. A data folder opened with login off holds a default admin that stays valid and
  skips the parameters' admin. `WEBUI_ADMIN_NAME`, `WEBUI_SECRET_KEY` and `DEFAULT_USER_ROLE` stay
  unbound.
* Configuration bindings set every Open WebUI environment value, persistent configuration is off,
  and `OPENAI_API_BASE_URL` gets the site's inference endpoint. Data lives at
  `/app/backend/data`.

## Update

1. `just image-lock ghcr.io/open-webui/open-webui:<version>` (locks linux/amd64 and linux/arm64).
2. Run the checks below, then set `version` in the catalog declaration.

## Validate

1. On a Docker apps site, deploy a model and Open WebUI, open the endpoint the console shows.
2. Sign in with the sidebar's email and password; unauthenticated access is refused.
3. The model list shows the site's routes; one chat returns an answer.
