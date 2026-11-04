{
  pkgs,
  backend,
  frontend,
  ingress,
  catalogRoot,
  catalogArtifacts,
  platformSystems,
  rev,
  source,
  root,
}:

let
  caddy = ingress.caddy;
  caddyfile = ingress.caddyfiles.prod;

  masterCatalog = import ./catalog.nix {
    inherit pkgs catalogRoot catalogArtifacts platformSystems;
  };
  catalog = masterCatalog.forRevision rev;

  # The flake source with its revision, from which the backend builds locked artifacts of that
  # revision; empty without a revision, like the catalog keeping `self`.
  packagedSource = pkgs.lib.optionalString (rev != null)
    "path:${source.outPath}?rev=${rev}&narHash=${source.narHash}";

  makeLauncher =
    {
      dotcCommand,
      caddyCommand,
      runtimeConfigCommand,
      frontendRoot,
      drainCommand,
      launcherSource ? packagedSource,
    }:
    pkgs.writeShellApplication {
      name = "dotcore";
      runtimeInputs = [
        backend
        caddy
        frontend.runtimeConfig
        pkgs.coreutils
        pkgs.jq
      ];
      derivationArgs = {
        postCheck = ''
          export DOTCORE_PUBLIC_PORT=8080
          export DOTCORE_MACHINE_HOST=127.0.0.1
          export DOTCORE_FRONTEND_STATIC_SERVER_ROOT=/tmp/dotcore-frontend-root
          export DOTCORE_MANAGEMENT_UPSTREAM=http://127.0.0.1:50051
          export DOTCORE_INFERENCE_UPSTREAM=http://127.0.0.1:50052
          ${pkgs.lib.getExe caddy} validate --config ${caddyfile} --adapter caddyfile
        '';
      };
      text = builtins.replaceStrings
        [ "@dotcCommand@" "@caddyCommand@" "@runtimeConfigCommand@" "@frontendRoot@" "@drainCommand@" "@caddyfile@" "@catalog@" "@packagedSource@" ]
        [ dotcCommand caddyCommand runtimeConfigCommand frontendRoot drainCommand (toString caddyfile) (toString catalog) launcherSource ]
        (builtins.readFile ./launcher.sh);
    };

  launcher = makeLauncher {
    dotcCommand = "${backend}/bin/dotc";
    caddyCommand = pkgs.lib.getExe caddy;
    runtimeConfigCommand = pkgs.lib.getExe frontend.runtimeConfig;
    frontendRoot = toString frontend.package;
    drainCommand = "${pkgs.coreutils}/bin/cat";
  };

  recordedCommand = role: executable: condition: pkgs.writeShellScriptBin "dotcore-record-${role}" ''
    set -euo pipefail
    if ${condition}; then
      printf '%s\n' "$$" > "''${DOTCORE_FAILURE_PID_DIR:?}/${role}.pid"
    fi
    exec ${executable} "$@"
  '';

  backendRecorder = recordedCommand "backend" "${backend}/bin/dotc" ''[[ " $* " == *" server start "* ]]'';
  caddyRecorder = recordedCommand "caddy" (pkgs.lib.getExe caddy) ''[[ "$1" == run ]]'';
  drainRecorder = recordedCommand "drain" "${pkgs.coreutils}/bin/cat" "true";

  recordedFixture = makeLauncher {
    dotcCommand = "${backendRecorder}/bin/dotcore-record-backend";
    caddyCommand = "${caddyRecorder}/bin/dotcore-record-caddy";
    runtimeConfigCommand = pkgs.lib.getExe frontend.runtimeConfig;
    frontendRoot = toString frontend.package;
    drainCommand = "${drainRecorder}/bin/dotcore-record-drain";
  };

  # Simulates an operator declining initialization: the command reports the cancellation
  # record and exits successfully without writing any site input. It records the private,
  # management and inference address variables that `site init` sees when asked to.
  noPublicationInit = pkgs.writeShellScriptBin "dotcore-init-noop" ''
    set -euo pipefail
    if [[ " $* " == *" config show "* ]]; then
      exec ${backend}/bin/dotc "$@"
    fi
    if [[ " $* " == *" site init "* ]]; then
      if [[ -n "''${DOTCORE_PACKAGE_INIT_ENV_FILE:-}" ]]; then
        printf '%s %s %s\n' "''${DOTCORE_PRIVATE_ADDR-unset}" "''${DOTCORE_MANAGEMENT_ADDR-unset}" \
          "''${DOTCORE_INFERENCE_ADDR-unset}" > "$DOTCORE_PACKAGE_INIT_ENV_FILE"
      fi
      printf '%s\n' "Site initialization cancelled"
      exit 0
    fi
    printf '%s\n' "unexpected command: $*" >&2
    exit 97
  '';

  # Simulates a backend that reports a node role the launcher does not know. It records the
  # packaged source the launcher hands the backend.
  unknownRoleConfig = pkgs.writeShellScriptBin "dotcore-config-unknown-role" ''
    set -euo pipefail
    if [[ " $* " == *" config show "* ]]; then
      printf '%s\n' "''${DOTCORE_PACKAGED_SOURCE_FLAKE-unset}" > "''${DOTCORE_PACKAGE_SOURCE_FILE:?}"
      printf '%s\n' '{"role":"bogus","siteDir":"/site","masterCatalogDir":"/catalog","cacheDir":"/cache"}'
      exit 0
    fi
    printf '%s\n' "unexpected command: $*" >&2
    exit 97
  '';

  unknownRoleFixture = makeLauncher {
    dotcCommand = "${unknownRoleConfig}/bin/dotcore-config-unknown-role";
    caddyCommand = pkgs.lib.getExe caddy;
    runtimeConfigCommand = pkgs.lib.getExe frontend.runtimeConfig;
    frontendRoot = toString frontend.package;
    drainCommand = "${pkgs.coreutils}/bin/cat";
    launcherSource = fixtureSource;
  };
  fixtureSource = "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source?rev=0123456789abcdef0123456789abcdef01234567";

  markerCommand = role: executable: pkgs.writeShellScriptBin "dotcore-marker-${role}" ''
    set -euo pipefail
    if [[ -n "''${DOTCORE_PACKAGE_MARKER_DIR:-}" ]]; then
      : > "''${DOTCORE_PACKAGE_MARKER_DIR}/${role}"
    fi
    exec ${executable} "$@"
  '';

  noPublicationFixture = makeLauncher {
    dotcCommand = "${noPublicationInit}/bin/dotcore-init-noop";
    caddyCommand = "${markerCommand "caddy" (pkgs.lib.getExe caddy)}/bin/dotcore-marker-caddy";
    runtimeConfigCommand = "${markerCommand "runtime-config" (pkgs.lib.getExe frontend.runtimeConfig)}/bin/dotcore-marker-runtime-config";
    frontendRoot = toString frontend.package;
    drainCommand = "${markerCommand "drain" "${pkgs.coreutils}/bin/cat"}/bin/dotcore-marker-drain";
  };

  realChildFailureTests = pkgs.runCommand "dotcore-all-in-one-real-child-failures" {
    nativeBuildInputs = [
      pkgs.bash
      pkgs.coreutils
      pkgs.curl
      pkgs.gnugrep
      pkgs.python3
      pkgs.shellcheck
    ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.procps ]
      ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.darwin.ps ];
  } ''
    shellcheck ${./real-child-failure.sh}
    export DOTCORE_FAILURE_LAUNCHER=${recordedFixture}/bin/dotcore
    export DOTCORE_FAILURE_SITE_FIXTURE=${root}/backend/fixtures/simulated-site
    export DOTCORE_FAILURE_CA_BUNDLE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
    export DOTCORE_FAILURE_BACKEND_COMMAND=${backend}/bin/dotc
    export DOTCORE_FAILURE_CADDY_COMMAND=${pkgs.lib.getExe caddy}
    export DOTCORE_FAILURE_DRAIN_COMMAND=${pkgs.coreutils}/bin/cat
    bash ${./real-child-failure.sh}
    touch "$out"
  '';

  packageTests = pkgs.runCommand "dotcore-all-in-one-package-tests" {
    nativeBuildInputs = [ pkgs.bash pkgs.coreutils pkgs.curl pkgs.gnugrep pkgs.python3 pkgs.shellcheck ];
  } ''
    shellcheck ${./package.sh}
    export DOTCORE_PACKAGE_LAUNCHER=${launcher}/bin/dotcore
    export DOTCORE_PACKAGE_BACKEND_COMMAND=${backend}/bin/dotc
    export DOTCORE_PACKAGE_SITE_FIXTURE=${root}/backend/fixtures/simulated-site
    export DOTCORE_PACKAGE_CA_BUNDLE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
    export DOTCORE_PACKAGE_CATALOG=${catalog}
    export DOTCORE_PACKAGE_INIT_FIXTURE_LAUNCHER=${noPublicationFixture}/bin/dotcore
    export DOTCORE_PACKAGE_UNKNOWN_ROLE_LAUNCHER=${unknownRoleFixture}/bin/dotcore
    export DOTCORE_PACKAGE_FIXTURE_SOURCE=${pkgs.lib.escapeShellArg fixtureSource}
    bash ${./package.sh}
    touch "$out"
  '';

  # Materializes the master catalog for a fixed revision, so workspace checks exercise the
  # committed-tree rewrite, and validates it with the backend catalog validator.
  catalogTestRevision = "0123456789abcdef0123456789abcdef01234567";
  testCatalog = masterCatalog.materialize catalogTestRevision;

  catalogTests = pkgs.runCommand "dotcore-master-catalog-tests" {
    nativeBuildInputs = [ pkgs.coreutils pkgs.findutils pkgs.jq pkgs.yq-go ];
    passthru.catalog = testCatalog;
  } ''
    catalog=${testCatalog}
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME"

    # Rewritten native artifacts carry one store path per selector platform and an explicit executable.
    rewritten=0
    while IFS= read -r -d "" release; do
      count=$(yq -o=json '.' "$release" | jq -e --arg flake "github:dotcoreinc/dotcore-oss/${catalogTestRevision}" \
        '[.artifacts[] | select(.nix.flake? == $flake and (.nix.dockerImage | not))]
          | if all((.nix.storePaths | length) == (.selectors | length) and (.nix.executable | type) == "string")
            then length else false end') \
        || { echo "incomplete rewritten Nix artifact in $release" >&2; exit 1; }
      rewritten=$((rewritten + count))
    done < <(find "$catalog/engines" -name '*.yaml' -print0)
    if [[ "$rewritten" -eq 0 ]]; then
      echo "no Nix artifact was rewritten to the test revision" >&2
      exit 1
    fi

    ${backend}/bin/dotc site validate \
      --site-dir ${root}/backend/fixtures/simulated-site --catalog-dir "$catalog"

    # Docker-delivered artifacts name exactly the store path of every system loading each declared
    # platform, as the package records it.
    images=0
    while IFS= read -r -d "" document; do
      count=$(yq -o=json '.' "$document" | jq -e --arg flake "github:dotcoreinc/dotcore-oss/${catalogTestRevision}" \
        --slurpfile artifacts ${masterCatalog.catalogArtifactsJson} \
        --slurpfile platformSystems ${masterCatalog.platformSystemsJson} '
          [(.artifacts[]? | {nix, platforms: [.selectors[].platform]}),
            (.graph.services[]? | {nix: .artifact.nix, platforms: .artifact.supportedPlatforms})]
          | map(select(.nix.dockerImage)
              | . as $image
              | ([$image.platforms[] | "\(.operatingSystem)/\(.architecture)"
                  | $platformSystems[0].dockerImage[.][]] | unique
                | map({system: ., storePath: $artifacts[0][$image.nix.package].systems[.].storePath})) as $expected
              | if $image.nix == {flake: $flake, package: $image.nix.package, dockerImage: {}, storePaths: $expected}
                then 1 else error("\($image.nix.package): \($image.nix), expected store paths \($expected)") end)
          | length') || { echo "unexpected Docker-delivered artifact in $document" >&2; exit 1; }
      images=$((images + count))
    done < <(find "$catalog/engines" "$catalog/applications" -name '*.yaml' -print0)
    if [[ "$images" -eq 0 ]]; then
      echo "no Docker-delivered artifact was materialized" >&2
      exit 1
    fi

    # A package that is not a Docker image derivation, or whose loader program is not its store entry
    # name, fails with a message naming it. An optional third argument replaces the artifact map.
    expect_failure() {
      local package=$1 expected=$2 artifacts=''${3:-${masterCatalog.catalogArtifactsJson}}
      if jq -n --arg package "$package" '{applicationId: "fixture", graph: {services: [{serviceName: "web",
          artifact: {nix: {flake: "self", package: $package, dockerImage: {}},
            supportedPlatforms: [{operatingSystem: "linux", architecture: "amd64"}]}}]}}' \
        | jq --arg repository github:dotcoreinc/dotcore-oss --arg rev ${catalogTestRevision} \
          --slurpfile artifacts "$artifacts" \
          --slurpfile platformSystems ${masterCatalog.platformSystemsJson} \
          --from-file ${./materialize-catalog.jq} > /dev/null 2> failure.log; then
        echo "materializing $package unexpectedly succeeded" >&2
        exit 1
      fi
      grep -qF -- "$expected" failure.log || { echo "unexpected failure: $(cat failure.log)" >&2; exit 1; }
    }
    expect_failure apps.missing.docker "the root flake has no package apps.missing.docker"
    expect_failure engines.llama-cpp.native "package engines.llama-cpp.native provides \`executable\`, but the artifact declares \`dockerImage\`"
    jq '.["apps.n8n.docker"].systems["x86_64-linux"].mainProgram = "docker-image-other"' \
      ${masterCatalog.catalogArtifactsJson} > renamed-loader.json
    expect_failure apps.n8n.docker "runs docker-image-other (meta.mainProgram), but nodes run the loader its store entry names, docker-image-n8n" renamed-loader.json
    touch "$out"
  '';

in
{
  package = launcher.overrideAttrs (old: {
    passthru = (old.passthru or { }) // {
      inherit caddyfile catalog;
      tests.default = packageTests;
      tests.failures = realChildFailureTests;
      tests.catalog = catalogTests;
    };
    meta = (old.meta or { }) // {
      mainProgram = "dotcore";
    };
  });
}
