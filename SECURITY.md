# Security policy

Dotcore is in alpha. Security is best effort for now and not guaranteed.

## Supported versions

There is no release yet. Security fixes land on `main` only, until the first release.

## Report a vulnerability

Don't open a public discussion, issue or pull request for a vulnerability.

- Report it privately through GitHub: on the repository's Security tab, choose "Report a vulnerability".
- If you can't use GitHub, email [security@dotcore.ai](mailto:security@dotcore.ai).

Include what you found, how to reproduce it, and the Dotcore revision you tested. We aim to reply within a week, and we'll tell you when a fix lands.

## Known limitations

These are planned work, not vulnerabilities to report:

- **No login or access control.** Anyone who can reach a site can use its models and apps and change its configuration.
- **Open ports.** The console and the inference endpoint listen on the address chosen during setup, with no authentication.
- **No isolation between users or apps.** Every user and app on a site can reach every model and app on it.

Run Dotcore only on a network you trust. [Install and set up](docs/install.md#security) explains which address to choose.
