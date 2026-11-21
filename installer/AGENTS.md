# Installer agentic instructions

`install.sh` takes a Linux or macOS host from no Dotcore to a running node: it checks Docker and
Nix, adds Dotcore to the user's Nix profile, then explains how to start it. `install.sh uninstall` removes it
again. [Installing Dotcore](../docs/install.md) is the user guide; `just --list installer` lists
the recipes.

## Script rules

- POSIX `sh` only: `curl | sh` runs under dash on Ubuntu, and macOS ships bash 3.2. Run
  `just installer::check` (shellcheck as `sh`) after every change.
- Keep all logic in functions and call `main "$@"` on the last line, so a partly downloaded script
  runs nothing.
- Prompts read from and write to `/dev/tty`: stdin is the script itself when piped from `curl`.
- Never print environment values.
- The installer only checks Docker. It never installs Docker, NVIDIA drivers or toolkits.
- Uninstall deletes the site only through `dotc site delete` or `dotc node leave`, which also
  remove the site's containers and native workloads, then offers `dotc gc` for the images the
  site used, before the downloads folder it reads. It removes the profile entry last because it
  holds `dotc`. It never removes the Nix store or Nix.

## Testing

- Never run a real install or uninstall against the user's own Nix profile or site. Run the
  installer regression cases with `just installer::test` (or `just installer::check`): they drive
  the real `install.sh` in a pseudo-terminal (`python3`'s `pty`, no third-party dependencies)
  inside a throwaway `HOME` with a curated PATH — fake `nix`, `uname`, `pgrep` and a fake
  `dotc` bundle, real `curl` against a fake Unix socket. The cases assert whether execution
  reaches the Nix/profile install or the fresh join, then stop before anything else is mutated.
  A real `nix profile add` is safe only with a throwaway `HOME`: its profile lives there.
- The regression cases fake only what they can control: a fake `uname` proves the probe takes
  the platform branch, never the real platform. A real `curl` against a fake Unix socket still
  exercises the real reachability probe. End-to-end installs remain checked by hand on a real host.
- A run from this repository's shell offers to use the environment's own `dotc` and `dotcore`
  instead of installing. Pick install only with a throwaway `HOME`, or stop and run the installer
  by path from a clean shell.
- End-to-end installs are checked by hand on a real host of each platform.
