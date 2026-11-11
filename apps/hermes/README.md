# Hermes application

[Hermes Agent](https://github.com/NousResearch/hermes-agent), an AI agent with tools, memory and a
web dashboard.

## Using it

* Open the dashboard from the console. Its address stays the same for the deployment's life.
* Log in with the username and password shown in the console's app panel.
* Chat with the agent on the dashboard's Chat page.
* Link an inference route to make it the default model. Without one, pick a route in the dashboard
  before chatting.

## Good to know

* The login is the same for every deployment, so it doesn't protect the dashboard from anyone who
  can reach it.
* The chat runs the agent with full tool and terminal access inside its container.
* Restarting the app logs you out.
* Dotcore doesn't manage its credentials or secrets.
* The Hermes agent API is off.

Maintaining it: [`AGENTS.md`](AGENTS.md).
