# Open WebUI agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image; nothing is built here.
* `WEBUI_AUTH` is off. Configuration bindings set every Open WebUI environment value, persistent
  configuration is off, and `OPENAI_API_BASE_URL` gets the site's inference endpoint. Data lives at
  `/app/backend/data`.

## Update

1. `just image-lock ghcr.io/open-webui/open-webui:<version>` (locks linux/amd64 and linux/arm64).
2. Run the checks below, then set `version` in the catalog declaration.

## Validate

1. On a Docker apps site, deploy a model and Open WebUI, open the endpoint the console shows.
2. The model list shows the site's routes; one chat returns an answer.
