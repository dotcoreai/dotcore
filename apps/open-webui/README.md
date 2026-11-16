# Open WebUI application

[Open WebUI](https://github.com/open-webui/open-webui), a chat interface for your models.

## Using it

* Open it from the console. Login is off by default, so it opens straight into the chat. It lists
  the site's inference routes as models.
* To require login, turn on "Require login" when you deploy, then sign in with the email and
  password in the deployment's sidebar.

## Good to know

* The admin email and password apply only when login is required. Sign-up is closed.
* The password is the admin's initial one. If you change it inside Open WebUI, the sidebar still
  shows the old one.
* Each start re-applies the settings Dotcore sets, so changes you make to them in the app don't
  last.
* Chats and settings live in the deployment's data.

Maintaining it: [`AGENTS.md`](AGENTS.md).
