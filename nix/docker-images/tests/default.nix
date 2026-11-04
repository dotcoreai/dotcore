{
  nixpkgs,
  system,
  systems,
}:

# Sandbox checks of the Docker image library: evaluation properties on inline fixtures, and the real
# loader run against fake Docker and Nix programs. The real-daemon smoke test lives in `smoke.sh`.
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  library =
    images: loaderInputs:
    import ../. (
      {
        inherit
          nixpkgs
          system
          systems
          images
          ;
      }
      // lib.optionalAttrs (loaderInputs != null) { inherit loaderInputs; }
    );

  fixtures = {
    fixture-busybox = ./busybox/docker-image.nix;
    fixture-build = ./build/docker-image.nix;
  };

  fakeDocker =
    name:
    pkgs.writeShellApplication {
      inherit name;
      runtimeInputs = with pkgs; [
        coreutils
        findutils
        gnutar
        jq
      ];
      text = builtins.readFile ./fake-docker.sh;
    };

  fakeNix = pkgs.writeShellApplication {
    name = "nix";
    runtimeInputs = [ pkgs.coreutils ];
    text = builtins.readFile ./fake-nix.sh;
  };

  fakeInputs =
    p: with p; [
      coreutils
      gnutar
      jq
      (fakeDocker "docker")
      (fakeDocker "docker-buildx")
    ];

  fakeLoaders = (library fixtures fakeInputs).packages;

  buildSpec =
    extra:
    { lib }:
    {
      dockerfile = (import ./build/docker-image.nix { inherit lib; }).dockerfile // extra;
    };

  # Dockerfile fixture with a current store build record, and its stored image: a `docker save`
  # layout built in the sandbox as a fixed-output path with the record's name and hash, which is the
  # path the record names. After changing the fixture, refresh each platform's hash: set it to
  # `lib.fakeHash`, run `just dev::docker-images-check` on that platform (arm64 through
  # `just dev::remote::run apple dev docker-images-check`), and copy the `got:` hash it reports.
  recordedBuild =
    let
      inherit (fakeLoaders.fixture-build.metadata) platform reference dockerfile;
      hashes = {
        "linux/amd64" = "sha256-r8Z+rJIFqKg93YizIDe8NKGRKKv9ELT9lPrmmClmceY=";
        "linux/arm64" = "sha256-D0rlYReX7daX5ePptLUQoaElWZprNonvmMhAJyq/usg=";
      };
      layer = "stored layer\n";
      diffIds = [ "sha256:${builtins.hashString "sha256" layer}" ];
      config = builtins.toJSON {
        os = "linux";
        architecture = lib.last (lib.splitString "/" platform);
        rootfs = {
          type = "layers";
          diff_ids = diffIds;
        };
        config.Labels."dotcore.recipe" = dockerfile.recipeHash;
      };
      manifest = builtins.toJSON {
        schemaVersion = 2;
        mediaType = "application/vnd.oci.image.manifest.v1+json";
        config = blob "application/vnd.oci.image.config.v1+json" config;
        layers = [ (blob "application/vnd.oci.image.layer.v1.tar" layer) ];
      };
      blob = mediaType: content: {
        inherit mediaType;
        digest = "sha256:${builtins.hashString "sha256" content}";
        size = lib.stringLength content;
      };
      records.${platform} = {
        inherit (dockerfile) recipeHash;
        hash = hashes.${platform};
        inherit diffIds;
      };
    in
    {
      inherit diffIds;
      loader =
        (library { fixture-build = buildSpec { inherit records; }; } fakeInputs).packages.fixture-build;
      layout =
        pkgs.runCommand dockerfile.storeName
          {
            outputHash = hashes.${platform};
            outputHashMode = "recursive";
            inherit layer config manifest;
            index = builtins.toJSON {
              schemaVersion = 2;
              mediaType = "application/vnd.oci.image.index.v1+json";
              manifests = [
                (
                  blob "application/vnd.oci.image.manifest.v1+json" manifest
                  // {
                    annotations = {
                      "io.containerd.image.name" = "docker.io/${reference}";
                      "org.opencontainers.image.ref.name" = lib.last (lib.splitString ":" reference);
                    };
                  }
                )
              ];
            };
            dockerManifest = builtins.toJSON [
              {
                Config = "blobs/sha256/${builtins.hashString "sha256" config}";
                RepoTags = [ reference ];
                Layers = [ "blobs/sha256/${builtins.hashString "sha256" layer}" ];
              }
            ];
            passAsFile = [
              "layer"
              "config"
              "manifest"
              "index"
              "dockerManifest"
            ];
          }
          ''
            mkdir -p $out/blobs/sha256
            cp $layerPath $out/blobs/sha256/${builtins.hashString "sha256" layer}
            cp $configPath $out/blobs/sha256/${builtins.hashString "sha256" config}
            cp $manifestPath $out/blobs/sha256/${builtins.hashString "sha256" manifest}
            cp $indexPath $out/index.json
            cp $dockerManifestPath $out/manifest.json
            echo '{"imageLayoutVersion":"1.0.0"}' > $out/oci-layout
          '';
    };

  loaders = (library fixtures null).packages;

  # Builds the Dockerfile fixture loader with the store build records the smoke test produces.
  smokeImage = pkgs.writeText "docker-images-smoke-image.nix" ''
    { records }:
    let
      nixpkgs = {
        lib = import ${nixpkgs}/lib;
        legacyPackages.${system} = import ${nixpkgs} { system = "${system}"; };
      };
    in
    (import ${../.} {
      inherit nixpkgs;
      system = "${system}";
      systems = [ "${system}" ];
      images.fixture-build =
        { lib }:
        {
          dockerfile = (import ${./build}/docker-image.nix { inherit lib; }).dockerfile // { inherit records; };
        };
    }).packages.fixture-build
  '';

  # Evaluation properties, each a message and a condition.
  evalAssertions =
    let
      lock = lib.importJSON ./busybox/docker-image.lock.json;
      platform = (library fixtures null).platform;
      packages = (library fixtures null).packages;
      registry = packages.fixture-busybox;
      build = packages.fixture-build;
      recipeHash = build.metadata.dockerfile.recipeHash;
      diffIds = [ "sha256:recorded" ];

      imageOf = spec: (library { fixture-build = spec; } null).packages.fixture-build;
      recordsOf = hash: {
        ${platform} = {
          recipeHash = hash;
          hash = lib.fakeHash;
          inherit diffIds;
        };
      };
      recorded = imageOf (buildSpec {
        records = recordsOf recipeHash;
      });
      stale = imageOf (buildSpec {
        records = recordsOf "0000";
      });
      otherArgs = imageOf (buildSpec {
        buildArgs.GREETING = "bye";
      });

      amd64Only =
        (library {
          fixture-build = buildSpec { platforms = [ "linux/amd64" ]; };
        } null).catalogArtifacts;

      badName = (library { Fixture_Build = buildSpec { }; } null).packages.Fixture_Build;

      tampered =
        (library {
          fixture-busybox =
            { }:
            {
              registry.lock = lib.recursiveUpdate lock {
                platforms.${platform}.manifest = lock.platforms.${platform}.manifest + " ";
              };
            };
        } null).packages.fixture-busybox;
    in
    [
      {
        message = "registry reference is tagged with the pinned manifest digest";
        ok =
          registry.metadata.reference
          == "dotcore-nix/fixture-busybox:${lib.removePrefix "sha256:" lock.platforms.${platform}.digest}";
      }
      {
        message = "registry identity is the locked diff IDs";
        ok = registry.metadata.identity.diffIds == lock.platforms.${platform}.diffIds;
      }
      {
        message = "registry stored image lists each blob once (config and layer)";
        ok = lib.length registry.metadata.stored.blobs == 2;
      }
      {
        message = "a lock manifest that does not hash to its digest fails evaluation";
        ok = !(builtins.tryEval (builtins.deepSeq tampered.metadata tampered.metadata)).success;
      }
      {
        message = "a Dockerfile image without a store build record has no stored image";
        ok = build.stored == null && build.metadata.stored == null && build.metadata.identity == null;
      }
      {
        message = "a current store build record names the stored image and its identity";
        ok =
          recorded.stored != null
          && lib.hasSuffix "-${recorded.metadata.dockerfile.storeName}" recorded.metadata.stored.layout
          && recorded.metadata.identity.diffIds == diffIds;
      }
      {
        message = "a stale store build record is ignored";
        ok = stale.stored == null && stale.metadata.identity == null;
      }
      {
        message = "the recipe hash ignores store build records and covers build arguments";
        ok =
          recorded.metadata.dockerfile.recipeHash == recipeHash
          && otherArgs.metadata.dockerfile.recipeHash != recipeHash;
      }
      {
        message = "the recipe hash ignores the loader's tools";
        ok = fakeLoaders.fixture-build.metadata.dockerfile.recipeHash == recipeHash;
      }
      {
        message = "an image name outside the Docker repository rule fails evaluation";
        ok = !(builtins.tryEval badName.outPath).success;
      }
      {
        message = "images exist only on systems loading one of their platforms";
        ok = lib.attrNames amd64Only.fixture-build == [ "x86_64-linux" ];
      }
    ];
in
rec {
  # Real-daemon smoke test of the fixtures; see `smoke.sh`.
  smoke = pkgs.writeShellApplication {
    name = "docker-images-smoke";
    runtimeInputs = with pkgs; [
      coreutils
      docker-client
      gnugrep
      jq
    ];
    text = ''
      export BUSYBOX=${loaders.fixture-busybox}
      # Realized with the smoke test so the stored-image step finds it in the store.
      export BUSYBOX_STORED=${loaders.fixture-busybox.stored}
      export BUILD=${loaders.fixture-build}
      export SMOKE_IMAGE=${smokeImage}
    ''
    + builtins.readFile ./smoke.sh;
  };

  evalCheck =
    let
      failed = lib.filter (assertion: !assertion.ok) evalAssertions;
    in
    if failed == [ ] then
      pkgs.runCommand "docker-images-eval-tests" { } "touch $out"
    else
      throw "docker image evaluation tests failed: ${lib.concatMapStringsSep "; " (a: a.message) failed}";

  loaderCheck =
    pkgs.runCommand "docker-images-loader-tests"
      {
        nativeBuildInputs = with pkgs; [
          coreutils
          diffutils
          gnugrep
          gnused
          jq
        ];
        REGISTRY = fakeLoaders.fixture-busybox;
        # The loader program without the package wrapper that passes the package store path.
        REGISTRY_LOADER = fakeLoaders.fixture-busybox.loader;
        DOCKERFILE = fakeLoaders.fixture-build;
        RECORDED = recordedBuild.loader;
        RECORDED_DIFF_IDS = builtins.toJSON recordedBuild.diffIds;
        # Realized so the recorded image's stored layout is in the store.
        RECORDED_STORED = recordedBuild.layout;
        # Realized so the stored-image scenario finds the blobs it streams.
        STORED = fakeLoaders.fixture-busybox.stored;
      }
      ''
        export NO_NIX_PATH=$PATH
        export PATH=${fakeNix}/bin:$PATH
        export SAMPLES=$out
        bash -euo pipefail ${./loader-test.sh}
      '';

  # The backend reads the loader's image metadata and result line from committed samples; fails
  # when the loader's formats gain, lose or rename a field. `just dev::docker-images-samples`
  # refreshes the samples.
  samplesCheck =
    pkgs.runCommand "docker-images-samples"
      {
        nativeBuildInputs = [ pkgs.jq ];
        committed = ../../../backend/fixtures/docker-image;
      }
      ''
        fields='[paths(scalars) | map(if type == "number" then "[]" else . end) | join(".")] | unique'
        for sample in registry-image.json dockerfile-image.json load-result.json; do
          if [[ "$(jq -c "$fields" "${loaderCheck}/$sample")" != "$(jq -c "$fields" "$committed/$sample")" ]]; then
            echo "$sample: the loader's fields differ from backend/fixtures/docker-image/$sample; update the backend's reader, then run just dev::docker-images-samples" >&2
            exit 1
          fi
        done
        touch $out
      '';
}
