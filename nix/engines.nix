{
  nixpkgs,
  system,
  systems,
  engines,
}:

let
  lib = nixpkgs.lib;

  # Engine packages of the evaluating system, keyed by engine name.
  packages = packagesFor system;

  # Engine name to Nix system to store path, main program, and version, for every system each
  # engine supports.
  catalogArtifacts = lib.zipAttrsWith (_: lib.mergeAttrsList) (
    map (
      targetSystem:
      lib.mapAttrs (_: package: { ${targetSystem} = packageArtifact package; }) (packagesFor targetSystem)
    ) systems
  );

  # Engine packages of one system, keyed by engine name, limited to the systems their
  # `meta.platforms` supports. Support is decided from the evaluating system's package metadata, so
  # unsupported systems never instantiate nixpkgs.
  packagesFor =
    targetSystem:
    lib.mapAttrs (_: package: enginePackage targetSystem package) (
      lib.filterAttrs (_: package: supports targetSystem package) engines
    );

  supports =
    targetSystem: package:
    lib.meta.availableOn (lib.systems.elaborate targetSystem) (enginePackage system package);

  # Engines build from the root nixpkgs without the development overlays, so evaluating one engine
  # package never fetches development-only inputs such as the Rust toolchain overlay.
  enginePackage =
    targetSystem: package: nixpkgs.legacyPackages.${targetSystem}.callPackage package { };

  # Evaluated store path, main program, and version of one engine package. String context is
  # discarded so consumers never build an engine for another system.
  packageArtifact = package: {
    storePath = builtins.unsafeDiscardStringContext package.outPath;
    mainProgram = package.meta.mainProgram or null;
    version = package.version or null;
  };
in
{
  inherit packages catalogArtifacts;

  # Platform to the Nix systems building native packages for it, for catalog materialization.
  # Mirrors `system_for` in backend/domain/src/nix.rs; change both together.
  nativeSystems = {
    "linux/amd64" = [ "x86_64-linux" ];
    "linux/arm64" = [ "aarch64-linux" ];
    "darwin/arm64" = [ "aarch64-darwin" ];
  };
}
