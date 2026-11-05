{
  pkgs,
  craneLib,
  root,
}:

let
  caddy = pkgs.caddy;
  frontendRoot = root + "/frontend";
  frontendCaddyfile = frontendRoot + "/Caddyfile";
  frontendCaddyArgs = "--config ${frontendCaddyfile} --adapter caddyfile";
  frontendRuntimeConfigSource = frontendRoot + "/tools/runtime-config.mjs";

  # Exclude local build outputs and site-specific generated runtime configuration.
  frontendSrc = pkgs.lib.cleanSourceWith {
    src = frontendRoot;
    filter =
      path: type:
      let
        baseName = pkgs.lib.baseNameOf path;
      in
      baseName != "node_modules"
      && baseName != "dist"
      && baseName != "coverage"
      && baseName != ".vite"
      && path != "${toString frontendRoot}/public/config.js";
  };

  # Package and test builds reuse the same fetched dependency derivation.
  frontendNpmDeps = pkgs.fetchNpmDeps {
    name = "dotcore-frontend-npm-deps";
    src = frontendSrc;
    hash = "sha256-n4PHja8lWdZt92w8L2IVFbeb86dgqwtGM8nwb81E/0Q=";
  };

  frontendArgs = {
    pname = "dotcore-frontend";
    version = "0.1.0";
    src = frontendSrc;
    npmDeps = frontendNpmDeps;
  };

  frontendTest = pkgs.buildNpmPackage (
    frontendArgs
    // {
      pname = "dotcore-frontend-tests";
      nativeBuildInputs = [
        pkgs.bashInteractive
        pkgs.just
      ];
      buildPhase = ''
        runHook preBuild
        export PATH="${pkgs.bashInteractive}/bin:${pkgs.just}/bin:$PATH"
        ln -s "$PWD" ../frontend
        printf "mod frontend 'frontend/Justfile'\\n" > ../Justfile
        sed -i 's|#!/usr/bin/env bash|#!${pkgs.bashInteractive}/bin/bash|g' Justfile
        npm run test -- --exclude tools/runtime-config.test.mjs
        node tools/runtime-config.test.mjs
        runHook postBuild
      '';
      installPhase = ''
        runHook preInstall
        touch "$out"
        runHook postInstall
      '';
    }
  );

  frontend = pkgs.buildNpmPackage (
    frontendArgs
    // {
      passthru.tests.default = frontendTest;
      meta.license = pkgs.lib.licenses.asl20;
      installPhase = ''
        runHook preInstall
        mkdir -p "$out"
        cp -R dist/. "$out/"
        runHook postInstall
      '';
    }
  );

  frontendRuntimeConfig = pkgs.writeShellApplication {
    name = "dotcore-frontend-runtime-config";
    runtimeInputs = [ pkgs.nodejs_24 ];
    text = ''
      exec node ${frontendRuntimeConfigSource} "$@"
    '';
  };

  frontendServer = pkgs.writeShellApplication {
    name = "dotcore-frontend-server";
    runtimeInputs = [ caddy ];
    # Caddy resolves the runtime placeholders while validating its configuration.
    derivationArgs.postCheck = ''
      export DOTCORE_FRONTEND_STATIC_SERVER_ROOT=/tmp/dotcore-frontend-root
      export DOTCORE_FRONTEND_STATIC_SERVER_HOST=127.0.0.1
      export DOTCORE_FRONTEND_STATIC_SERVER_PORT=3000
      ${pkgs.lib.getExe caddy} validate ${frontendCaddyArgs}
    '';
    text = ''
      if [ "$#" -ne 3 ]; then
        echo "usage: dotcore-frontend-server <document-root> <host> <port>" >&2
        exit 2
      fi

      if [ ! -d "$1" ]; then
        echo "document root is not a directory: $1" >&2
        exit 2
      fi

      export DOTCORE_FRONTEND_STATIC_SERVER_ROOT="$1"
      export DOTCORE_FRONTEND_STATIC_SERVER_HOST="$2"
      export DOTCORE_FRONTEND_STATIC_SERVER_PORT="$3"

      exec caddy run ${frontendCaddyArgs}
    '';
  };
in
{
  package = frontend;
  test = frontendTest;
  server = frontendServer;
  runtimeConfig = frontendRuntimeConfig;
}
