{
  nixpkgs,
  system,
  systems,
  images,
  # Programs the loader runs, as a function of the system's packages. Tests substitute fakes.
  loaderInputs ?
    pkgs: with pkgs; [
      coreutils
      docker-buildx
      docker-client
      gnutar
      jq
    ],
}:

# Docker image derivations: one package per image and Nix system, keyed by image name, whose
# derivation and loader program are named `docker-image-<name>`. A package is a cheap loader that
# makes the image available in a Docker daemon, from its stored image (image bytes in the Nix store,
# servable by a binary cache) when available, otherwise from its source: a pull by digest from the
# upstream registry, or a local Docker build of a pinned context.
let
  lib = nixpkgs.lib;

  # Docker image platform each Nix system loads. Docker hosts on macOS run Docker Desktop, whose
  # images are Linux images for the host architecture.
  platforms = {
    x86_64-linux = "linux/amd64";
    aarch64-linux = "linux/arm64";
    aarch64-darwin = "linux/arm64";
  };

  # Image packages of the evaluating system, keyed by image name.
  packages = packagesFor system;

  # Image name to Nix system to store path, image platform, and loader program, for every
  # system each image supports. String context is discarded so consumers never build a loader for
  # another system.
  catalogArtifacts = lib.zipAttrsWith (_: lib.mergeAttrsList) (
    map (
      targetSystem:
      lib.mapAttrs (_: package: {
        ${targetSystem} = {
          storePath = builtins.unsafeDiscardStringContext package.outPath;
          platform = package.metadata.platform;
          mainProgram = package.meta.mainProgram;
        };
      }) (packagesFor targetSystem)
    ) systems
  );

  # Image packages of one system, limited to the images declaring that system's platform.
  packagesFor =
    targetSystem:
    let
      pkgs = nixpkgs.legacyPackages.${targetSystem};
      platform = platforms.${targetSystem} or null;
      specs = lib.mapAttrs (_: file: pkgs.callPackage file { }) images;
    in
    lib.optionalAttrs (platform != null) (
      lib.mapAttrs (name: spec: mkImage pkgs platform name spec) (
        lib.filterAttrs (_: spec: lib.elem platform (specPlatforms spec)) specs
      )
    );

  # Docker repository path component rule; the launch reference repository embeds the image name.
  dockerNamePattern = "[a-z0-9]+(([._]|__|-+)[a-z0-9]+)*";

  specPlatforms =
    spec:
    if spec ? registry then
      lib.attrNames (registryLock spec.registry).platforms
    else
      spec.dockerfile.platforms;

  # Loader package of one image for one platform, with the stored image as `passthru.stored` (null
  # when the image has none for this platform) and its metadata as `passthru.metadata`. The package
  # passes its own store path to the loader, which tags the image with a reference derived from it.
  # Its derivation name is also its loader program: the backend derives both the program and the
  # launch reference from the store entry name, so the catalog names only the package.
  mkImage =
    pkgs: platform: name: spec:
    assert lib.assertMsg (
      builtins.match dockerNamePattern name != null
    ) "docker image ${name}: names must match the Docker repository rule ${dockerNamePattern}";
    let
      source =
        if spec ? registry then
          registrySource pkgs platform name spec.registry
        else
          dockerfileSource pkgs platform name spec.dockerfile;
      # `share/dotcore/image.json`. The backend reads `reference`, `platform`, `identity.diffIds` and
      # `dockerfile.recipeHash`, and the loader's result line; `backend/fixtures/docker-image` holds
      # samples of both, which `checks.docker-images-samples` compares field by field.
      metadata = {
        inherit name platform;
        inherit (source) kind reference identity;
        staleRecord = source.staleRecord or false;
        stored = source.storedMetadata;
      }
      // source.extra;
      drvName = "docker-image-${name}";
      metadataFile = pkgs.writeText "${drvName}.json" (builtins.toJSON metadata);
      loader = pkgs.writeShellApplication {
        name = drvName;
        runtimeInputs = loaderInputs pkgs;
        text = ''
          IMAGE_JSON=${metadataFile}
        ''
        + builtins.readFile ./loader.sh;
      };
    in
    pkgs.runCommand drvName
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
        passthru = {
          inherit metadata loader;
          stored = source.stored;
        };
        meta = {
          description = "Loads the ${name} Docker image (${platform}) into a Docker daemon";
          mainProgram = drvName;
        };
      }
      ''
        mkdir -p $out/bin $out/share/dotcore
        makeWrapper ${loader}/bin/${drvName} $out/bin/${drvName} \
          --set DOCKER_IMAGE_PACKAGE $out
        ln -s ${metadataFile} $out/share/dotcore/image.json
      '';

  # Upstream image pinned by a lock file holding, per platform, the exact manifest bytes (whose hash
  # is the pinned digest) and the layer diff IDs used as identity.
  registrySource =
    pkgs: platform: name: registry:
    let
      lock = registryLock registry;
      entry = lock.platforms.${platform};
      digest = "sha256:${builtins.hashString "sha256" entry.manifest}";
      manifest = builtins.fromJSON entry.manifest;
      # Manifests may repeat a layer, such as the shared empty layer.
      blobDigests = lib.unique ([ manifest.config.digest ] ++ map (layer: layer.digest) manifest.layers);
      blobs = map (blobDigest: {
        digest = blobDigest;
        path = registryBlob pkgs lock.repository blobDigest;
      }) blobDigests;
    in
    assert lib.assertMsg (
      entry.digest == digest
    ) "docker image ${name}: lock manifest for ${platform} hashes to ${digest}, not ${entry.digest}";
    {
      kind = "registry";
      reference = "dotcore-nix/${name}:${lib.removePrefix "sha256:" digest}";
      identity.diffIds = entry.diffIds;
      stored = pkgs.linkFarm "docker-image-${name}-stored" (
        map (blob: {
          name = lib.removePrefix "sha256:" blob.digest;
          inherit (blob) path;
        }) blobs
      );
      # Blob paths stay out of the loader's closure, so building a loader never fetches image bytes.
      storedMetadata = {
        manifest = "${pkgs.writeText "docker-image-${name}-manifest.json" entry.manifest}";
        blobs = map (blob: {
          inherit (blob) digest;
          path = builtins.unsafeDiscardStringContext "${blob.path}";
        }) blobs;
      };
      extra.registry = {
        inherit (lock) repository;
        inherit digest;
      };
    };

  # Locks are a JSON file or an attribute set.
  registryLock =
    registry: if builtins.isAttrs registry.lock then registry.lock else lib.importJSON registry.lock;

  # One registry blob as a fixed-output path keyed only by its digest, so the same layer is stored
  # once across images and versions.
  registryBlob =
    pkgs: repository: digest:
    pkgs.runCommand "docker-blob-${lib.removePrefix "sha256:" digest}"
      {
        nativeBuildInputs = [
          pkgs.cacert
          pkgs.crane
        ];
        outputHash = digest;
        outputHashMode = "flat";
        impureEnvVars = lib.fetchers.proxyImpureEnvVars;
      }
      ''
        crane blob ${repository}@${digest} > $out
      '';

  # Prints the lock file of an upstream image for the registry sources' `lock`.
  lockTool =
    let
      pkgs = nixpkgs.legacyPackages.${system};
    in
    pkgs.writeShellApplication {
      name = "docker-images-lock";
      runtimeInputs = with pkgs; [
        coreutils
        crane
        jq
      ];
      text = builtins.readFile ./lock.sh;
    };

  # Image built by Docker from a pinned context. A stored image exists only for platforms with a
  # store build record matching the current recipe; otherwise the loader builds locally.
  dockerfileSource =
    pkgs: platform: name: dockerfile:
    let
      file = dockerfile.file or "Dockerfile";
      buildArgs = dockerfile.buildArgs or { };
      target = dockerfile.target or null;
      context = "${dockerfile.context}";
      recipeHash = builtins.substring 0 32 (
        builtins.hashString "sha256" (
          builtins.toJSON {
            inherit
              file
              buildArgs
              target
              platform
              ;
            context = builtins.unsafeDiscardStringContext context;
          }
        )
      );
      storeName = "docker-image-${name}-${lib.replaceStrings [ "/" ] [ "-" ] platform}-${recipeHash}";
      # Records are a JSON file, absent until the first store build, or an attribute set.
      records =
        if builtins.isAttrs (dockerfile.records or null) then
          dockerfile.records
        else if dockerfile ? records && builtins.pathExists dockerfile.records then
          lib.importJSON dockerfile.records
        else
          { };
      record = records.${platform} or null;
      current = record != null && record.recipeHash == recipeHash;
      stored =
        if current then
          pkgs.requireFile {
            name = storeName;
            inherit (record) hash;
            hashMode = "recursive";
            message = ''
              The stored ${name} image for ${platform} is not in the Nix store or any substituter.
              Run the store build mode on a ${platform} Docker host: docker-image-${name} store-build
            '';
          }
        else
          null;
    in
    {
      kind = "dockerfile";
      reference = "dotcore-nix/${name}:${recipeHash}";
      staleRecord = record != null && !current;
      identity = if current then { inherit (record) diffIds; } else null;
      inherit stored;
      storedMetadata =
        if current then { layout = builtins.unsafeDiscardStringContext stored.outPath; } else null;
      extra.dockerfile = {
        inherit
          context
          file
          buildArgs
          target
          recipeHash
          storeName
          ;
      };
    };
in
{
  inherit packages catalogArtifacts lockTool;

  # Docker image platform to the Nix systems loading it, for catalog materialization.
  imageSystems = lib.foldlAttrs (
    acc: system: platform:
    acc // { ${platform} = (acc.${platform} or [ ]) ++ [ system ]; }
  ) { } platforms;

  # Fails while any image has a stale store build record for any platform, evaluated on every
  # system without building.
  recordsCheck =
    let
      pkgs = nixpkgs.legacyPackages.${system};
      stale = lib.filter (package: package.metadata.staleRecord) (
        lib.concatMap (targetSystem: lib.attrValues (packagesFor targetSystem)) systems
      );
    in
    if stale == [ ] then
      pkgs.runCommand "docker-images-records" { } "touch $out"
    else
      throw "stale Docker image store build records, rerun their store build mode: ${
        lib.concatMapStringsSep ", " (package: "${package.name} (${package.metadata.platform})") stale
      }";

  # Docker image platform of the evaluating system, or null when it loads no images.
  platform = platforms.${system} or null;
}
