# n8n application

[n8n](https://github.com/n8n-io/n8n), workflow automation that can call your models.

## Using it

* Open it from the console and sign in with the login it shows (`admin@dotcore.local` / `admin`).
* A ready-made `Dotcore Inference` credential connects n8n's OpenAI nodes to your models.
* In the OpenAI node's "Message a model", turn off the "Store" option. Dotcore doesn't store
  responses, so the request fails with it on.

## Webhooks

* Webhooks use the app's address and have no login. Their path is the only guard.
* Webhook URLs use the machine's address from when the app was deployed. If that address changes,
  deploy the app again.

## Good to know

* The login is fixed and public, so keep the app on a trusted network.
* Workflows, credentials and settings live in the deployment's data. Losing it loses them.
* Telemetry, version notifications and the AI Assistant are off.

Maintaining it: [`AGENTS.md`](AGENTS.md).
