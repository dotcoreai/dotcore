{
  description = "Dotcore frontend, protobuf, and Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      crane,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        lib = pkgs.lib;

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ ];
          targets = [ ];
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        root = ./.;

        backend = import ./nix/backend.nix {
          inherit
            pkgs
            craneLib
            root
            ;
        };
        frontend = import ./nix/frontend.nix {
          inherit pkgs craneLib root;
        };
        ingress = import ./nix/ingress.nix { inherit pkgs; };

        # Engine and app builds by folder: native packages and Docker image derivations. They are
        # exposed as `legacyPackages.<system>.<area>.<name>.<kind>`, the attribute path catalog Nix
        # artifacts name as their package.
        engineNatives = {
          llama-cpp = ./engines/llama-cpp/package.nix;
        };
        engineImages = {
          llama-cpp = ./engines/llama-cpp/docker-image.nix;
          ninfer = ./engines/ninfer/docker-image.nix;
          strata = ./engines/strata/docker-image.nix;
          vllm = ./engines/vllm/docker-image.nix;
        };
        appImages = {
          comfyui = ./apps/comfyui/docker-image.nix;
          hermes = ./apps/hermes/docker-image.nix;
          n8n = ./apps/n8n/docker-image.nix;
          open-webui = ./apps/open-webui/docker-image.nix;
        };

        engines = import ./nix/engines.nix {
          inherit nixpkgs system;
          systems = flake-utils.lib.defaultSystems;
          engines = engineNatives;
        };

        # Derivation names and launch references are keyed by image name only, so names are unique
        # across areas.
        dockerImages =
          assert lib.assertMsg (lib.intersectAttrs engineImages appImages == { })
            "Docker image names must be unique across apps and engines: ${lib.concatStringsSep ", " (lib.attrNames (lib.intersectAttrs engineImages appImages))}";
          import ./nix/docker-images {
            inherit nixpkgs system;
            systems = flake-utils.lib.defaultSystems;
            images = engineImages // appImages;
          };

        # Area to build name to kind to the value a library keyed by name holds for it (package or
        # catalog artifact), omitting builds the library lacks.
        buildTree =
          natives: images:
          let
            area =
              names: areaNatives:
              lib.filterAttrs (_: kinds: kinds != { }) (
                lib.genAttrs names (
                  name:
                  lib.optionalAttrs (areaNatives ? ${name}) { native = areaNatives.${name}; }
                  // lib.optionalAttrs (images ? ${name}) { docker = images.${name}; }
                )
              );
          in
          {
            engines = area (lib.attrNames (engineNatives // engineImages)) natives;
            apps = area (lib.attrNames appImages) { };
          };

        # Build tree leaves keyed by attribute path, as catalog materialization looks packages up.
        byPath =
          tree:
          lib.concatMapAttrs (
            area: lib.concatMapAttrs (name: lib.mapAttrs' (kind: lib.nameValuePair "${area}.${name}.${kind}"))
          ) tree;

        dockerImageTests = import ./nix/docker-images/tests {
          inherit nixpkgs system;
          systems = flake-utils.lib.defaultSystems;
        };

        allInOne = import ./nix/all-in-one {
          inherit
            pkgs
            frontend
            ingress
            root
            ;
          backend = backend.package;
          catalogRoot = ./catalog;
          # Package attribute path to what it provides (the catalog field naming it) and its
          # per-system artifacts; platform to the Nix systems building each kind.
          catalogArtifacts =
            let
              provide = provides: lib.mapAttrs (_: systems: { inherit provides systems; });
            in
            byPath (
              buildTree (provide "executable" engines.catalogArtifacts) (
                provide "dockerImage" dockerImages.catalogArtifacts
              )
            );
          platformSystems = {
            executable = engines.nativeSystems;
            dockerImage = dockerImages.imageSystems;
          };
          # Absent when the tree has uncommitted changes.
          rev = self.rev or null;
          source = self;
        };

        apps = import ./apps { inherit pkgs craneLib root; };
      in
      {
        packages = {
          backend = backend.package;
          dotc = backend.package;
          frontend = frontend.package;
          frontend-server = frontend.server;
          dotcore = allInOne.package;
          # Links only dotcore and the catalog-aware dotc so profile installs expose both without
          # dotcore-launcher.
          default = pkgs.runCommand "dotcore-bundle" { meta.mainProgram = "dotcore"; } ''
            mkdir -p $out/bin
            ln -s ${allInOne.package}/bin/dotcore $out/bin/dotcore
            ln -s ${allInOne.package}/bin/dotc $out/bin/dotc
          '';
        };

        # Nested engine, app and tool builds; `packages` only allows derivations at its top level.
        legacyPackages = buildTree engines.packages dockerImages.packages // {
          tools = {
            docker-images-lock = dockerImages.lockTool;
          }
          // lib.optionalAttrs (dockerImages.platform != null) {
            docker-images-smoke = dockerImageTests.smoke;
          };
        };

        checks = {
          backend-tests = backend.package.tests.default;
          frontend-tests = frontend.package.tests.default;
          frontend-server = frontend.server;
          dotcore = allInOne.package;
          dotcore-tests = allInOne.package.tests.default;
          dotcore-failures = allInOne.package.tests.failures;
          dotcore-catalog = allInOne.package.tests.catalog;
          ingress = ingress.check;
          apps = apps.check;
        }
        // pkgs.lib.optionalAttrs (dockerImages.platform != null) {
          docker-images-eval = dockerImageTests.evalCheck;
          docker-images-loader = dockerImageTests.loaderCheck;
          docker-images-samples = dockerImageTests.samplesCheck;
          docker-images-records = dockerImages.recordsCheck;
        }
        // {
          # `nix flake check` does not evaluate `legacyPackages`: instantiate every leaf of every
          # system without building it.
          # Also checks that no build reads source metadata (`rev`, `lastModified`): a node builds a
          # locked revision from local source or from GitHub, which must give the same store paths.
          legacy-packages-eval =
            let
              drvPaths =
                flake:
                lib.concatMap (
                  targetSystem:
                  map (package: builtins.unsafeDiscardStringContext package.drvPath) (
                    lib.collect lib.isDerivation flake.legacyPackages.${targetSystem}
                  )
                ) flake-utils.lib.defaultSystems;
              withoutMetadata = builtins.getFlake (
                builtins.unsafeDiscardStringContext "path:${self.outPath}?narHash=${self.narHash}"
              );
            in
            assert lib.assertMsg (drvPaths self == drvPaths withoutMetadata)
              "a legacyPackages build reads flake source metadata (rev, lastModified), so builds of one revision from local source and from GitHub would differ";
            pkgs.runCommand "legacy-packages-eval" { } "touch $out";
        };

        devShells.default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              just

              backend.cli
              backend.dotcoreCli

              nodejs_24

              ingress.caddy
              ingress.server

              flock
              buf
              jq
              grpcurl
              shellcheck
              actionlint
              process-compose
              protobuf
              protoc-gen-es
              gitleaks
              trufflehog
            ]

            # macOS: put the unwrapped cargo first in the dev shell. rust-overlay wraps cargo to
            # export DYLD_LIBRARY_PATH to Nix's curl, because the system curl that cargo links reads
            # outside the Nix build sandbox (rust-overlay PRs #251 and #254). Every process cargo
            # starts inherits the variable, and dyld then searches that directory first for each
            # library it loads: about 30ms per process. nextest runs every test in its own process,
            # so a full backend run pays it once per test (~2,700 times). Outside the sandbox the
            # system curl works, and Nix builds keep the wrapper.
            ++ lib.optionals stdenv.isDarwin [ rust-bin.stable.latest.cargo ]
            ++ [
              rustToolchain
              cargo-nextest
              cargo-outdated

              clang # backend
            ];

          # Buf discovers the local ECMAScript generator through PATH.
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          PROTOC_INCLUDE = "${pkgs.protobuf}/include";
        };
      }
    );
}
