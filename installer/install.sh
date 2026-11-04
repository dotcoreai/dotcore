#!/bin/sh
# Dotcore installer: takes a Linux or macOS host from no Dotcore to a running node. It checks
# Docker and Nix, adds Dotcore to the user's Nix profile, then tells you how to start it. Its
# `uninstall` command removes Dotcore again, asking before it deletes any data.
#
# Run it by path from a Dotcore checkout, or piped from curl once the repository is public.
# `install.sh --help` lists the commands and options.

set -eu

DEFAULT_FLAKE_REF="github:dotcoreinc/dotcore"
DETERMINATE_INSTALL_URL="https://install.determinate.systems/nix"
DETERMINATE_DOCS_URL="https://docs.determinate.systems/determinate-nix/"
NIX_UNINSTALL_DOCS_URL="https://nix.dev/manual/nix/stable/installation/uninstall.html"
NIX_DAEMON_PROFILE="/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh"
DEFAULT_DOCKER_ENDPOINT="unix:///var/run/docker.sock"

# Runs the requested command after the checks both share.
main() {
  detect_colors
  parse_options "$@"

  check_terminal
  check_development_environment

  case "$command" in
    install) run_install ;;
    uninstall) run_uninstall ;;
  esac
}

# Installs Dotcore, one step after the other; each step stops the run on a failure it can't fix.
run_install() {
  step "Checking Docker"
  check_docker

  step "Checking Nix"
  ensure_nix

  step "Choosing the install source"
  select_source

  step "Looking for an existing install"
  detect_existing_install

  step "Installing Dotcore"
  install_profile

  step "Next steps"
  print_next_steps
}

# Removes Dotcore: its runtime resources and site folder, its cache folder, and its Nix profile
# entry, asking before each deletion. Docker images, the Nix store and Nix itself stay.
run_uninstall() {
  step "Looking for Dotcore"
  if ! find_installation; then
    info "Dotcore isn't installed here. Nothing to do."
    return 0
  fi
  check_dotcore_stopped

  ask_yes_no "Uninstall Dotcore? It asks before deleting your site and downloads." no || exit 0

  remove_installation

  step "Left on this host"
  print_leftovers
}

usage() {
  cat <<EOF
Usage: install.sh [--flake-ref <ref>]
       install.sh uninstall

Commands:
  (none)             Install or update Dotcore in your Nix profile, then explain how
                     to start it.
  uninstall          Remove Dotcore from this host, asking before deleting your site and
                     downloads.

Options:
  --flake-ref <ref>  Flake to install. Default: the Dotcore checkout holding this script, else
                     $DEFAULT_FLAKE_REF.
                     Variable: DOTCORE_INSTALL_FLAKE_REF
  -h, --help         Show this help.

The installer asks before every choice, so it needs a terminal.
EOF
}

parse_options() {
  command=install
  flake_ref="${DOTCORE_INSTALL_FLAKE_REF:-}"

  while [ $# -gt 0 ]; do
    case "$1" in
      uninstall)
        command=uninstall
        shift
        ;;
      --flake-ref)
        [ $# -ge 2 ] && [ -n "$2" ] || fail "--flake-ref needs a flake reference."
        flake_ref="$2"
        shift 2
        ;;
      --flake-ref=*)
        flake_ref="${1#--flake-ref=}"
        [ -n "$flake_ref" ] || fail "--flake-ref needs a flake reference."
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        fail "Unknown option: $1"
        ;;
    esac
  done
}

# Asks before going on when the Dotcore development environment is active: its settings would
# point the run, and the started node, at a development site instead of the user's own. Dotcore
# settings the user sets on purpose, and other direnv or Nix shell environments, don't count.
check_development_environment() {
  # Only the development workspace environment sets this variable; Dotcore itself never reads it.
  [ -z "${DOTCORE_DEV_INDEX+x}" ] && return 0

  warn "The Dotcore development environment is active. Its settings, such as its site folder and"
  warn "ports, apply to this run and to Dotcore once it starts."
  ask_yes_no "Continue with the development environment's settings?" no && return 0

  hint "To run it from a clean shell:"
  hint "  1. Leave the Dotcore checkout folder (for example 'cd ~') so direnv unloads it, or exit"
  hint "     the Nix development shell."
  hint "  2. Run the installer by its path, for example '<checkout>/installer/install.sh'. It still"
  hint "     installs from that checkout."
  exit 1
}

check_terminal() {
  if ! (: </dev/tty) 2>/dev/null; then
    fail "The installer asks questions, so it needs a terminal. Run it from an interactive shell."
  fi
}

# Checks that this user reaches the Docker daemon Dotcore will use, without sudo. Dotcore talks to
# DOCKER_HOST when it is a Unix socket, else to the system socket, never to a Docker context, so
# the check targets that same endpoint.
check_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    error "Docker isn't installed. Dotcore runs models and apps in Docker."
    docker_install_remedy
    exit 1
  fi

  endpoint=$(docker_endpoint)

  if output=$(DOCKER_HOST="$endpoint" docker version --format '{{.Server.Version}}' 2>&1) && [ -n "$output" ]; then
    success "Docker $output is reachable at $endpoint."
    return 0
  fi

  case "$output" in
    *"permission denied"* | *"Permission denied"*)
      error "You can't use Docker without sudo: permission denied on $endpoint."
      docker_permission_remedy
      ;;
    *)
      error "Docker isn't reachable at $endpoint:"
      printf '  %s\n' "$output" >&2
      docker_daemon_remedy
      ;;
  esac
  exit 1
}

# Finds Nix, or installs it with the Determinate installer when the user agrees, then checks that
# flakes work.
ensure_nix() {
  if ! command -v nix >/dev/null 2>&1 && [ -r "$NIX_DAEMON_PROFILE" ]; then
    load_nix_profile
  fi

  if ! command -v nix >/dev/null 2>&1; then
    info "Nix isn't installed. Dotcore needs it to install and to build what it runs."
    choice=$(ask_choice "How do you want to get Nix?" manual \
      "auto:Install it now with the Determinate Nix installer (asks for sudo)" \
      "manual:Show how to install it myself, then stop" \
      "stop:Stop now")

    case "$choice" in
      auto) install_nix ;;
      manual)
        info "Install Nix by following $DETERMINATE_DOCS_URL"
        info "Then run the installer again."
        exit 0
        ;;
      stop) exit 0 ;;
    esac
  fi

  if ! nix eval --expr 1 >/dev/null 2>&1; then
    error "Nix is installed, but flakes and the nix command aren't enabled."
    cat >&2 <<EOF
Add this line to ~/.config/nix/nix.conf, then run the installer again:
  experimental-features = nix-command flakes
EOF
    exit 1
  fi

  success "$(nix --version) is ready."
}

install_nix() {
  command -v curl >/dev/null 2>&1 || fail "curl is needed to download the Nix installer. Install curl, then run the installer again."

  # sudo reads the password from the terminal itself; stdin stays the downloaded installer.
  info "Running the Determinate Nix installer. It asks for your password to use sudo."
  if ! curl -fsSL "$DETERMINATE_INSTALL_URL" | sh -s -- install --no-confirm; then
    fail "The Nix install failed. See its output above, or install Nix by following $DETERMINATE_DOCS_URL"
  fi

  [ -r "$NIX_DAEMON_PROFILE" ] || fail "Nix was installed, but $NIX_DAEMON_PROFILE is missing. Open a new terminal and run the installer again."
  load_nix_profile
  command -v nix >/dev/null 2>&1 || fail "Nix was installed, but isn't on PATH yet. Open a new terminal and run the installer again."
}

# Picks the flake to install: an explicit reference, else the checkout holding this script, else
# the Dotcore repository. A checkout install reads the local repository, so it needs no GitHub
# access.
select_source() {
  if [ -n "$flake_ref" ]; then
    info "Installing from $flake_ref, as requested."
  elif checkout_root=$(find_checkout_root); then
    flake_ref="git+file://$checkout_root"
    info "Installing from the Dotcore checkout at $checkout_root, which holds this installer."
    warn "Commit your changes first: an install from uncommitted changes can't deploy models or apps."
    return 0
  else
    flake_ref="$DEFAULT_FLAKE_REF"
    info "Installing from $flake_ref."
  fi

  if ! nix flake metadata "$flake_ref" >/dev/null 2>&1; then
    error "Can't read $flake_ref."
    cat >&2 <<EOF
If it's the Dotcore repository, it may be private. Clone it with an account that has access, then
run the installer from the checkout:
  <checkout>/installer/install.sh
EOF
    exit 1
  fi
}

# Looks for Dotcore already on this host and asks whether to update it, remove it and install it
# again, or stop. Sets `profile_entry` and `profile_entry_url` when the Nix profile holds Dotcore
# and the install keeps it.
detect_existing_install() {
  profile_entry=""
  profile_entry_url=""
  found=false

  entry=$(find_profile_entry)
  if [ -n "$entry" ]; then
    profile_entry=$(printf '%s\n' "$entry" | cut -f1)
    profile_entry_url=$(printf '%s\n' "$entry" | cut -f2)
    info "Your Nix profile has Dotcore as '$profile_entry', from $profile_entry_url."
    found=true
  fi

  for command in dotcore dotc; do
    if path=$(command -v "$command" 2>/dev/null); then
      info "'$command' is on your PATH at $path."
      found=true
    fi
  done

  site_dir="${XDG_STATE_HOME:-$HOME/.local/state}/dotcore/site"
  if [ -d "$site_dir" ]; then
    info "A Dotcore site exists at $site_dir. Installing keeps it."
    found=true
  fi

  if ! "$found"; then
    info "None found."
    return 0
  fi

  choice=$(ask_choice "Dotcore is already here. What do you want to do?" update \
    "update:Update Dotcore and keep your site" \
    "reinstall:Remove Dotcore, asking before deleting your site and downloads, then install it again" \
    "cancel:Stop without changing anything")
  case "$choice" in
    cancel) exit 0 ;;
    reinstall)
      step "Removing Dotcore"
      if find_installation; then
        check_dotcore_stopped
        remove_installation
      fi
      profile_entry=""
      profile_entry_url=""
      ;;
  esac
  return 0
}

# Adds Dotcore to the user's Nix profile, or updates the existing entry. An entry from another
# source is replaced, so the profile follows the chosen source.
install_profile() {
  info "Nix builds Dotcore from source, which can take a while the first time."

  if [ -n "$profile_entry" ] && [ "$profile_entry_url" = "$flake_ref" ]; then
    nix profile upgrade "$profile_entry" || fail "Updating '$profile_entry' failed. See the Nix output above."
  else
    if [ -n "$profile_entry" ]; then
      info "Replacing '$profile_entry' from $profile_entry_url with $flake_ref."
      nix profile remove "$profile_entry" || fail "Removing '$profile_entry' failed. See the Nix output above."
    fi
    # `nix profile add` replaced `nix profile install` in Nix 2.30; older versions only know install.
    if nix profile add --help >/dev/null 2>&1; then
      nix profile add "$flake_ref" || fail "Adding $flake_ref to your Nix profile failed. See the Nix output above."
    else
      nix profile install "$flake_ref" || fail "Adding $flake_ref to your Nix profile failed. See the Nix output above."
    fi
  fi

  entry=$(find_profile_entry)
  [ -n "$entry" ] || fail "Dotcore isn't in your Nix profile after the install. Check 'nix profile list'."
  bundle=$(printf '%s\n' "$entry" | cut -f3)
  "$bundle/bin/dotc" --help >/dev/null 2>&1 || fail "The installed dotc doesn't run: $bundle/bin/dotc --help failed."

  success "Dotcore is installed: 'dotcore' and 'dotc' are in your Nix profile."
}

# Tells the user how to continue: start this machine as a site manager, or add it to an existing
# site from its manager. A host with a site already set up only needs to start again.
print_next_steps() {
  if ! command -v dotcore >/dev/null 2>&1; then
    info "Open a new terminal first to have them on your PATH."
    printf '\n'
  fi

  if dotcore_running; then
    info "Dotcore is running and keeps its old version until you restart it."
    info "Stop it (Ctrl-C in its terminal), then run:"
    command_line "dotcore"
  elif [ -d "${XDG_STATE_HOME:-$HOME/.local/state}/dotcore/site" ]; then
    info "Your site is kept. Start Dotcore with:"
    command_line "dotcore"
  else
    info "To start this machine as the manager of a new site, run:"
    command_line "dotcore"
    info "To add this machine to an existing site instead:"
    info "  1. On the manager, stop Dotcore, then run: $(style 1 '1;36' 'dotc node add')"
    info "  2. Run the 'dotc node join' command it prints, here."
  fi
}

# Finds what Dotcore left on this host and reports it with folder sizes, so the user sees what
# large data is at stake. Fails when nothing is found. Sets `uninstall_entry`, `dotc_command`,
# `site_dir`, `site_role` and `cache_dir`; `dotc` reports the folders and role, else the defaults
# apply.
find_installation() {
  if ! command -v nix >/dev/null 2>&1 && [ -r "$NIX_DAEMON_PROFILE" ]; then
    load_nix_profile
  fi

  uninstall_entry=""
  dotc_command=""
  if command -v nix >/dev/null 2>&1; then
    entry=$(find_profile_entry)
    if [ -n "$entry" ]; then
      uninstall_entry=$(printf '%s\n' "$entry" | cut -f1)
      dotc_command="$(printf '%s\n' "$entry" | cut -f3)/bin/dotc"
    fi
  fi
  if [ -z "$dotc_command" ]; then
    dotc_command=$(command -v dotc 2>/dev/null || true)
  fi

  site_dir="${XDG_STATE_HOME:-$HOME/.local/state}/dotcore/site"
  cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/dotcore"
  site_role=manager
  if [ -n "$dotc_command" ] && config=$("$dotc_command" config show --format json 2>/dev/null); then
    site_dir=$(json_field "$config" siteDir)
    cache_dir=$(json_field "$config" cacheDir)
    site_role=$(json_field "$config" role)
  fi

  found=false
  if [ -n "$uninstall_entry" ]; then
    info "Nix profile entry: '$uninstall_entry'"
    found=true
  fi
  if [ -d "$site_dir" ]; then
    info "Site folder ($site_role): $site_dir ($(folder_size "$site_dir"))"
    found=true
  fi
  if [ -d "$cache_dir" ]; then
    info "Downloads, such as model weights: $cache_dir ($(folder_size "$cache_dir"))"
    found=true
  fi
  "$found"
}

check_dotcore_stopped() {
  if dotcore_running; then
    fail "Dotcore is running. Stop it (Ctrl-C in its terminal), then run this again."
  fi
}

# Deletes what `find_installation` found, asking before the site and the downloads. The site goes
# first, through dotc, because that also removes its containers and native workloads; the profile
# entry, which holds dotc, goes last.
remove_installation() {
  if [ -d "$site_dir" ]; then
    if ! ask_yes_no "Delete your site? It holds the site's settings, keys and app data." no; then
      warn "Kept $site_dir. Its containers and native workloads stay too."
    elif [ -z "$dotc_command" ]; then
      fail "Can't delete the site: dotc isn't installed. Install Dotcore again, then uninstall it."
    else
      delete_site
    fi
  fi

  if [ -d "$cache_dir" ]; then
    if ! ask_yes_no "Delete your downloads ($(folder_size "$cache_dir"))? Model weights take long to download again." no; then
      info "Kept $cache_dir."
    else
      delete_cache
    fi
  fi

  if [ -n "$uninstall_entry" ]; then
    nix profile remove "$uninstall_entry" || fail "Removing '$uninstall_entry' from your Nix profile failed. See the Nix output above."
    success "Removed Dotcore from your Nix profile."
  fi
}

# Deletes the site through dotc by role: a manager deletes its site, a joined node leaves it. Both
# list the runtime resources and confirm on their own; a decline stops the uninstall here, so the
# profile entry and dotc stay for another try. Then removes the Docker images the site used.
delete_site() {
  if [ "$site_role" = joined-node ]; then
    "$dotc_command" node leave --site-dir "$site_dir" </dev/tty || fail "The node didn't leave its site. See the output above."
  else
    "$dotc_command" site delete --site-dir "$site_dir" </dev/tty || fail "The site wasn't deleted. See the output above."
  fi
  success "Deleted the site and its runtime resources."

  # The site's Nix roots went with its folder, so its images are now unused; images another site on
  # this machine still uses stay. gc lists the images and confirms on its own, and reads the
  # downloads folder, so it runs before that folder can be deleted.
  if "$dotc_command" gc --cache-dir "$cache_dir" </dev/tty; then
    success "Removed the Docker images Dotcore no longer uses."
  else
    warn "Docker images stay. Remove them later with: docker image ls 'dotcore-nix/*'"
  fi
}

delete_cache() {
  # Guard against a wrong path before deleting a whole folder.
  case "$cache_dir" in
    */dotcore) ;;
    *) fail "Won't delete $cache_dir: it isn't a dotcore folder. Delete it yourself if it's right." ;;
  esac
  rm -rf "$cache_dir"
  success "Deleted $cache_dir."
}

# Prints what the uninstall leaves behind and how to remove it. Docker images are counted, since
# gc may already have removed them all; they remain when the site was kept, gc was declined, or
# another site on this machine uses them.
print_leftovers() {
  info "These stay. Remove them yourself if you don't need them:"
  images=$(DOCKER_HOST=$(docker_endpoint) docker image ls --quiet --filter 'reference=dotcore-nix/*' 2>/dev/null | wc -l | tr -d ' ') || images=0
  if [ "${images:-0}" -gt 0 ]; then
    info "  $images Docker images Dotcore loaded: docker image ls 'dotcore-nix/*'"
  fi
  info "  Docker build cache, if Docker built images here: docker builder prune"
  info "  Nix store space Dotcore used: nix store gc"
  # Only the Determinate installer leaves a receipt; on NixOS, Nix is part of the system.
  if [ -f /nix/receipt.json ]; then
    info "  Nix itself, installed with the Determinate installer: /nix/nix-installer uninstall"
  elif [ ! -e /etc/NIXOS ]; then
    info "  Nix itself: remove it the way you installed it. Nix's own installer: $NIX_UNINSTALL_DOCS_URL"
  fi
}

# Prints the Docker daemon endpoint Dotcore uses: DOCKER_HOST when it is a Unix socket, else the
# system socket, never a Docker context.
docker_endpoint() {
  case "${DOCKER_HOST:-}" in
    unix://*) printf '%s\n' "$DOCKER_HOST" ;;
    *) printf '%s\n' "$DEFAULT_DOCKER_ENDPOINT" ;;
  esac
}

dotcore_running() {
  pgrep -u "$(id -u)" -f '/bin/dotcore( |$)' >/dev/null 2>&1
}

# Prints a string field of a one-line JSON object; enough for dotc's own output, whose paths hold
# no escaped quotes.
json_field() {
  printf '%s\n' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"
}

folder_size() {
  du -sh "$1" 2>/dev/null | cut -f1
}

# Prints the Dotcore profile entry as `name<TAB>original URL<TAB>store path`, or nothing. Entries
# are named after their flake URL (a checkout's folder name), so Dotcore is found by its bundle
# store path instead. Nix parses the profile's JSON because the host may have no jq.
find_profile_entry() {
  profile_json=$(nix profile list --json 2>/dev/null) || return 0
  # The single-quoted text is a Nix expression, not shell.
  # shellcheck disable=SC2016
  installer_profile_json="$profile_json" nix eval --raw --impure --expr '
    let
      elements = (builtins.fromJSON (builtins.getEnv "installer_profile_json")).elements;
      bundle = name: builtins.filter (path: builtins.match ".*-dotcore-bundle" path != null)
        (elements.${name}.storePaths or [ ]);
      names = builtins.filter (name: bundle name != [ ]) (builtins.attrNames elements);
      line = name: "${name}\t${elements.${name}.originalUrl or ""}\t${builtins.head (bundle name)}\n";
    in
    builtins.concatStringsSep "" (map line names)
  ' 2>/dev/null | head -n 1
}

# Prints the root of the Dotcore checkout holding this script, or fails when the script doesn't
# run from a checkout (piped from curl, `$0` names the shell, not a file).
find_checkout_root() {
  [ -f "$0" ] || return 1
  script_dir=$(cd -P "$(dirname "$0")" 2>/dev/null && pwd -P) || return 1
  [ "$(basename "$script_dir")" = installer ] || return 1
  root=$(dirname "$script_dir")
  [ -f "$root/flake.nix" ] && [ -e "$root/.git" ] || return 1
  printf '%s\n' "$root"
}

load_nix_profile() {
  # The Nix profile script reads variables that may be unset.
  set +u
  # shellcheck source=/dev/null
  . "$NIX_DAEMON_PROFILE"
  set -u
}

docker_install_remedy() {
  if [ "$(uname -s)" = Darwin ]; then
    hint "Install Docker Desktop: https://docs.docker.com/desktop/setup/install/mac-install/"
  else
    hint "Install Docker Engine: https://docs.docker.com/engine/install/"
  fi
  hint "Then run the installer again."
}

docker_permission_remedy() {
  hint "Add yourself to the docker group:"
  hint "  sudo usermod -aG docker $(id -un)"
  hint "Then log out fully, or reboot, and run the installer again. Open sessions keep their old groups."
}

docker_daemon_remedy() {
  if [ "$(uname -s)" = Darwin ]; then
    hint "Start Docker Desktop. In its Settings > Advanced, allow the default Docker socket:"
    hint "Dotcore uses /var/run/docker.sock."
  else
    hint "Start the Docker daemon, for example: sudo systemctl start docker"
  fi
  hint "Then run the installer again."
}

# Asks a yes or no question on the terminal and succeeds on yes. An empty answer picks the default,
# `yes` or `no`.
ask_yes_no() {
  question="$1"
  default="$2"
  if [ "$default" = yes ]; then
    keys="Y/n"
  else
    keys="y/N"
  fi

  while true; do
    printf '\n%s %s ' "$(style tty 1 "$question")" "$(style tty 36 "[$keys]")" >/dev/tty
    IFS= read -r answer </dev/tty || fail "No answer: the terminal closed."

    case "${answer:-$default}" in
      [Yy] | [Yy][Ee][Ss]) return 0 ;;
      [Nn] | [Nn][Oo]) return 1 ;;
    esac
    printf '%s\n' "$(style tty 33 "Answer y or n.")" >/dev/tty
  done
}

# Asks a question on the terminal and prints the chosen key. Each option is `key:description`; an
# empty answer picks the default key.
ask_choice() {
  question="$1"
  default="$2"
  shift 2

  while true; do
    {
      printf '\n%s\n' "$(style tty 1 "$question")"
      index=1
      for option in "$@"; do
        marker=""
        [ "${option%%:*}" = "$default" ] && marker=" $(style tty 2 "(default)")"
        printf '  %s %s%s\n' "$(style tty 36 "$index)")" "${option#*:}" "$marker"
        index=$((index + 1))
      done
      printf '%s ' "$(style tty 1 "Choice [1-$#]:")"
    } >/dev/tty

    IFS= read -r answer </dev/tty || fail "No answer: the terminal closed."

    if [ -z "$answer" ]; then
      printf '%s\n' "$default"
      return 0
    fi
    index=1
    for option in "$@"; do
      if [ "$answer" = "$index" ]; then
        printf '%s\n' "${option%%:*}"
        return 0
      fi
      index=$((index + 1))
    done
    printf '%s\n' "$(style tty 33 "Pick a number from 1 to $#.")" >/dev/tty
  done
}

step() {
  printf '\n%s %s\n' "$(style 1 '1;34' '==>')" "$(style 1 1 "$1")"
}

info() {
  printf '    %s\n' "$1"
}

command_line() {
  printf '\n        %s\n\n' "$(style 1 '1;36' "$1")"
}

success() {
  printf '    %s\n' "$(style 1 32 "$1")"
}

warn() {
  printf '    %s\n' "$(style 1 33 "$1")"
}

hint() {
  printf '    %s\n' "$1" >&2
}

error() {
  printf '\n%s %s\n' "$(style 2 '1;31' 'Error:')" "$(style 2 1 "$1")" >&2
}

fail() {
  error "$1"
  exit 1
}

# Decides once which outputs show colors: inside a command substitution, stdout is a pipe even when
# the text ends up on the terminal. NO_COLOR and a dumb terminal turn colors off.
detect_colors() {
  color_stdout=false
  color_stderr=false
  color_tty=false
  if [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != dumb ]; then
    color_tty=true
    [ -t 1 ] && color_stdout=true
    [ -t 2 ] && color_stderr=true
  fi
  return 0
}

# Prints text with ANSI SGR codes (such as `1;34`) when its target shows colors: 1 for stdout, 2
# for stderr, `tty` for prompts.
style() {
  case "$1" in
    1) enabled="$color_stdout" ;;
    2) enabled="$color_stderr" ;;
    *) enabled="$color_tty" ;;
  esac

  if "$enabled"; then
    printf '\033[%sm%s\033[0m' "$2" "$3"
  else
    printf '%s' "$3"
  fi
}

main "$@"
