# Remote hosts

Some work needs hardware or an OS your machine lacks, such as an NVIDIA GPU or a Mac. A remote host is one of your own machines that has it. `just dev::remote::run` runs any `just` command there, on your current workspace with uncommitted edits, and streams the output back.

```sh
just dev::remote::list                          # which hosts are usable
just dev::remote::run gpu backend test-nvidia   # run a recipe on one
```

`just --list dev::remote` shows every command.

## Set up a host

The tool doesn't set up machines. Each host needs:

- A jj checkout of this repository, with `direnv allow` run in it.
- `.workspaces` in the remote user's global gitignore.
- Its ssh host key accepted once, by hand.

Then list your hosts in a JSON file, and point `DOTCORE_REMOTE_CONFIG` at it, for example from a parent `.envrc`:

```json
{
  "groups": {
    "gpu": {
      "user": "me",
      "dir": "/home/me/dotcore",
      "hosts": { "gpu1": "gpu1.example.net", "gpu2": "gpu2.example.net" }
    },
    "mac": { "dir": "/Users/me/dotcore", "hosts": { "mac1": "mac1.example.net" } }
  }
}
```

A host is written `<group>` or `<group>:<host>`. A bare group means its first host.

## What gets sent

Each run sends your working copy to the host over ssh, into a workspace of its own. Everything that isn't gitignored goes, including private commits and any secrets that aren't ignored. Nothing is pushed to `origin`.

The remote workspace has its own ports, site and settings, made by direnv on the host. Nothing from your local environment is forwarded.

## Clean up

`just dev::remote::clean <host>` removes your workspace from a host. Without a host, it does so for every host you configured. `just clean` offers it too.
