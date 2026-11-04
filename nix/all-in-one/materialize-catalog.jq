# Rewrites the development-source (`self`) Nix artifacts of one engine release or application to
# the locked Dotcore revision, with precomputed store paths, so the packaged master catalog never
# needs a development source.
#
# Each artifact gets the store path of every Nix system building one of its declared platforms
# (engine selector platforms or application supported platforms), and must agree with its package:
# * Native packages (`executable`): a `self` release always builds the current package, so its
#   version must equal that package's version, and its executable must equal the package's
#   `meta.mainProgram`.
# * Docker image derivations (`dockerImage`): each must load exactly the declared platform, and its
#   `meta.mainProgram` must equal its store entry name, which nodes run as the loader.
#
# Arguments: --arg repository (for example `github:dotcoreinc/dotcore-oss`), --arg rev, and
# --slurpfile for artifacts (package attribute path -> what it provides, the catalog field
# `executable` or `dockerImage`, and per Nix system its store path, main program, and version or
# image platform) and platformSystems (what a package provides -> platform -> Nix systems building
# it), both from flake.nix. jq requires definitions before the main expression.

def locked($storePaths):
  .nix.flake = "\($repository)/\($rev)"
  | .nix.storePaths = $storePaths;

# Input: an object holding the `self` artifact's `nix`; $platforms are its declared platforms, and
# $version the engine release version (null for applications).
def materialize($where; $platforms; $version):
  (if .nix.dockerImage then "dockerImage" else "executable" end) as $provides
  | (.nix.package // error("\($where): declare the flake package to build")) as $package
  | ($artifacts[0][$package] // error("\($where): the root flake has no package \($package)")) as $artifact
  | if $artifact.provides != $provides then
      error("\($where): package \($package) provides `\($artifact.provides)`, but the artifact declares `\($provides)`")
    else . end
  | (if $provides == "executable" then
       .nix.executable // error("\($where): declare the executable the package runs")
     else null end) as $executable
  | ([
      [$platforms[] | "\(.operatingSystem)/\(.architecture)"] | unique | .[] as $platform
      | ($platformSystems[0][$provides][$platform]
          // error("\($where): no Nix system builds `\($provides)` packages for platform \($platform)"))[] as $system
      | ($artifact.systems[$system]
          // error("\($where): package \($package) is not available for \($system), which serves \($platform)"))
      | if $provides == "executable" then
          if .version != $version then
            error("\($where): engine package \($package) is version \(.version), but the release declares version \($version). A self release always builds the current package: set the release version to the package version, or pin this release to the locked \($repository)/<rev> revision that built version \($version)")
          elif .mainProgram != $executable then
            error("\($where): engine package \($package) for \($system) runs \(.mainProgram) (meta.mainProgram), but the artifact declares executable \($executable); make them equal")
          else . end
        else
          (.storePath | split("/") | last | .[33:]) as $entryName
          | if .platform != $platform then
              error("\($where): Docker image package \($package) for \($system) loads \(.platform), but the artifact declares \($platform)")
            elif .mainProgram != $entryName then
              error("\($where): Docker image package \($package) for \($system) runs \(.mainProgram) (meta.mainProgram), but nodes run the loader its store entry names, \($entryName); make them equal")
            else . end
        end
      | {system: $system, storePath}
    ] | unique_by(.system)) as $storePaths
  | locked($storePaths);

def development_source: (.nix.flake? // "") | startswith("self");

if has("engineReleaseId") then
  .engineReleaseId as $release
  | .version as $version
  | .artifacts |= map(
      "engine release \($release) artifact \(.artifactId)" as $where
      | if development_source | not then .
        elif .nix.flake != "self" then
          error("\($where): unsupported development-source flake reference \(.nix.flake)")
        else materialize($where; [.selectors[].platform]; $version)
        end
    )
else
  .applicationId as $application
  | .graph.services |= map(
      "application \($application) service \(.serviceName)" as $where
      | if .artifact | development_source | not then .
        elif .artifact.nix.flake != "self" then
          error("\($where): unsupported development-source flake reference \(.artifact.nix.flake)")
        else .artifact |= materialize($where; .supportedPlatforms; null)
        end
    )
end
