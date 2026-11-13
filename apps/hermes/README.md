# Hermes application

[Hermes Agent](https://github.com/NousResearch/hermes-agent), an AI agent with tools, memory and a
web dashboard.

## Using it

* Open the dashboard from the console. Its address stays the same for the deployment's life.
* Log in with the username and password in the deployment's sidebar. The password is generated at
  deploy unless you type one.
* Chat with the agent on the dashboard's Chat page.
* Link an inference route to make it the default model. Without one, pick a route in the dashboard
  before chatting.

## Good to know

* The chat runs the agent with full tool and terminal access inside its container.
* Restarting the app logs you out.
* The Hermes agent API is off.
* Hermes keeps its data (sessions, memory, settings) inside the app. It is lost when the app is
  redeployed or recreated.

Maintaining it: [`AGENTS.md`](AGENTS.md).
