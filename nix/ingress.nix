{ pkgs }:

# Public ingress shared by the packaged `dotcore` and the development stack. One Caddy build serves
# the console, the management API and the inference API on a single origin; only the console
# handler differs: static files in the package, a proxy to the frontend server in development.
let
  inherit (pkgs) lib;

  caddy = pkgs.caddy;

  # Management and inference handlers shared by every generated Caddyfile.
  routes = builtins.readFile ./routes.caddy;

  caddyfiles = {
    prod = mkCaddyfile {
      name = "dotcore-ingress-prod-Caddyfile";
      consoleHandler = ''
        root * "{$DOTCORE_FRONTEND_STATIC_SERVER_ROOT}"
        try_files {path} /index.html
        file_server
      '';
    };

    dev = mkCaddyfile {
      name = "dotcore-ingress-dev-Caddyfile";
      consoleHandler = ''
        reverse_proxy {$DOTCORE_FRONTEND_UPSTREAM}
      '';
    };
  };

  # Runs the development ingress in the foreground. The upstreams are full HTTP origins.
  server = pkgs.writeShellApplication {
    name = "dotcore-ingress";
    runtimeInputs = [ caddy ];
    text = ''
      for variable in DOTCORE_MACHINE_HOST DOTCORE_PUBLIC_PORT DOTCORE_MANAGEMENT_UPSTREAM \
        DOTCORE_INFERENCE_UPSTREAM DOTCORE_FRONTEND_UPSTREAM; do
        if [ -z "''${!variable:-}" ]; then
          echo "dotcore-ingress: $variable must be set" >&2
          exit 2
        fi
      done

      echo "dotcore-ingress: console at http://$DOTCORE_MACHINE_HOST:$DOTCORE_PUBLIC_PORT" >&2

      exec caddy run --config ${caddyfiles.dev} --adapter caddyfile
    '';
  };

  # Validates every generated Caddyfile and requires Caddy's canonical formatting, so inlined routes
  # keep their structure.
  check = pkgs.runCommand "dotcore-ingress-check" { nativeBuildInputs = [ caddy pkgs.diffutils ]; } ''
    export HOME="$TMPDIR"
    export DOTCORE_PUBLIC_PORT=8080
    export DOTCORE_MACHINE_HOST=127.0.0.1
    export DOTCORE_FRONTEND_STATIC_SERVER_ROOT=/tmp/dotcore-frontend-root
    export DOTCORE_MANAGEMENT_UPSTREAM=http://127.0.0.1:50051
    export DOTCORE_INFERENCE_UPSTREAM=http://127.0.0.1:50052
    export DOTCORE_FRONTEND_UPSTREAM=http://127.0.0.1:5173
    for caddyfile in ${caddyfiles.prod} ${caddyfiles.dev}; do
      caddy validate --config "$caddyfile" --adapter caddyfile
      caddy fmt "$caddyfile" | diff -u "$caddyfile" -
    done
    touch "$out"
  '';

  # Builds a Caddyfile serving the shared routes on the public port, and the console for every
  # other path. It listens only on the machine host, never on every interface.
  mkCaddyfile =
    {
      name,
      consoleHandler,
    }:
    pkgs.writeText name (
      lib.concatStringsSep "\n" [
        "{"
        "\tadmin off"
        "\tauto_https off"
        # Several workspace stacks and the package run their own Caddy; none resumes a saved config.
        "\tpersist_config off"
        "}"
        ""
        "http://:{$DOTCORE_PUBLIC_PORT} {"
        (indent (
          "bind {$DOTCORE_MACHINE_HOST}\n\n"
          + routes
          + "\nhandle {\n"
          + indent consoleHandler
          + "\n}\n"
        ))
        "}"
        ""
      ]
    );

  # Indents every non-empty line by one tab, Caddy's canonical block indentation.
  indent =
    text:
    lib.concatMapStringsSep "\n" (line: if line == "" then "" else "\t" + line) (
      lib.splitString "\n" (lib.removeSuffix "\n" text)
    );
in
{
  inherit
    caddy
    caddyfiles
    server
    check
    ;
}
