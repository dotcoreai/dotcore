{
  pkgs,
  catalogRoot,
  catalogArtifacts,
  platformSystems,
}:

let
  lib = pkgs.lib;

  repository = "github:dotcoreinc/dotcore-oss";

  # Context-free strings: writing darwin store paths here never builds a darwin engine or loader.
  catalogArtifactsJson = pkgs.writeText "dotcore-catalog-artifacts.json" (
    builtins.toJSON catalogArtifacts
  );
  platformSystemsJson = pkgs.writeText "dotcore-platform-systems.json" (
    builtins.toJSON platformSystems
  );

  # Copies a catalog into the store. With a revision, every development-source (`self`) Nix
  # artifact of an engine release or application becomes the locked `github:` reference of that
  # revision with precomputed store paths. The build fails when a rewritten artifact disagrees with
  # its package (`materialize-catalog.jq`) or when any `self` artifact remains.
  materializeRoot =
    root: rev:
    pkgs.runCommand "dotcore-master-catalog"
      {
        nativeBuildInputs = [
          pkgs.findutils
          pkgs.jq
          pkgs.yq-go
        ];
      }
      ''
        mkdir -p "$out"
        cp -R ${root}/. "$out/"
        chmod -R u+w "$out"
        ${lib.optionalString (rev != null) ''
          # `self` Nix artifacts of engine artifacts and application services, by document kind.
          selfArtifacts='[(.artifacts[]? | select((.nix.flake? // "") | startswith("self")) | .artifactId),
            (.graph.services[]? | select((.artifact.nix.flake? // "") | startswith("self")) | .serviceName)]'
          for directory in engines applications; do
            [[ -d "$out/$directory" ]] || continue
            while IFS= read -r -d "" document; do
              # JSON is valid restricted YAML; only documents with `self` artifacts are rewritten.
              yq -o=json '.' "$document" > document.json
              if jq -e "$selfArtifacts | length > 0" document.json > /dev/null; then
                jq --arg repository ${repository} --arg rev ${lib.escapeShellArg rev} \
                  --slurpfile artifacts ${catalogArtifactsJson} \
                  --slurpfile platformSystems ${platformSystemsJson} \
                  --from-file ${./materialize-catalog.jq} document.json > "$document"
              fi

              remaining=$(yq -o=json '.' "$document" | jq -r "$selfArtifacts | join(\", \")")
              if [[ -n "$remaining" ]]; then
                echo "Master catalog document $document still has development-source Nix artifacts: $remaining" >&2
                exit 1
              fi
            done < <(find "$out/$directory" -name '*.yaml' -print0)
          done
        ''}
      '';

  materialize = materializeRoot catalogRoot;
in
{
  inherit
    materialize
    materializeRoot
    catalogArtifactsJson
    platformSystemsJson
    ;

  # Packaged master catalog for the flake source revision, or the unchanged development-source
  # catalog with a warning when the tree has no revision (uncommitted changes).
  forRevision =
    rev:
    if rev != null then
      materialize rev
    else
      lib.warn "dotcore: source tree has no revision (uncommitted changes); the packaged master catalog keeps development-source (self) Nix engine artifacts" (
        materialize null
      );
}
