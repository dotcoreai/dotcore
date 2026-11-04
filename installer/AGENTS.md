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

- Never run a real install or uninstall against the user's own Nix profile or site. Run checks in
  a clean environment (`env -i` with a throwaway `HOME`) and fake `docker`, `nix` and `dotc`
  commands that stop before `nix profile add` changes anything. A real `nix profile add` is safe
  only with a throwaway `HOME`: its profile lives there. A pseudo-terminal (`script`) provides the
  terminal the prompts need, and the environment must be set inside the command it runs.
- A run from this repository's shell asks whether to use the development settings; answer stop,
  or run the installer by path from a clean shell.
- End-to-end installs are checked by hand on a real host of each platform.
