{
  pkgs,
  craneLib,
  root,
}:

let
  appDirectories = pkgs.lib.filterAttrs (
    name: type:
    type == "directory" && builtins.pathExists (./. + "/${name}/default.nix")
  ) (builtins.readDir ./.);
  appChecks = pkgs.lib.mapAttrs (
    name: _:
    (import (./. + "/${name}") { inherit pkgs craneLib root; }).check
  ) appDirectories;
in
{
  checks = appChecks;
  check = pkgs.linkFarm "dotcore-app-checks" (
    pkgs.lib.mapAttrsToList (name: path: { inherit name path; }) appChecks
  );
}
