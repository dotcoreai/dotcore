{
  lib,
  stdenvNoCC,
  fetchurl,
  _7zz,
}:

# Packages the upstream macOS 26-27 release app untouched: rewriting its Mach-O files would
# invalidate their arm64 signatures, so the fixup phase is disabled. The package launcher runs with
# the bundle's own Python.
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "omlx";
  version = "0.8.4";

  src = fetchurl {
    url = "https://github.com/jundot/omlx/releases/download/v${finalAttrs.version}/oMLX-${finalAttrs.version}-macos26-27.dmg";
    hash = "sha256-RFw9QrGsXb3R2PHq3KCtTjXd7YRgJrPKPYU33Xzgigs=";
  };

  nativeBuildInputs = [ _7zz ];

  # `undmg` rejects this image; `-snld` keeps the bundle's relative symlinks.
  unpackPhase = ''
    runHook preUnpack
    7zz x -snld -y "$src" >/dev/null
    runHook postUnpack
  '';

  dontConfigure = true;
  dontBuild = true;
  dontFixup = true;
  dontUpdateAutotoolsGnuConfigScripts = true;

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/libexec" "$out/bin"
    cp -R oMLX.app "$out/libexec/oMLX.app"

    # 7zz stores the code-signature extended attributes of scripts as side files; they mean
    # nothing in the store.
    find "$out/libexec" -name '*:com.apple.*' -delete

    ln -s "$out/libexec/oMLX.app/Contents/MacOS/omlx-cli" "$out/bin/omlx"

    install -m 0644 ${./launcher.py} "$out/libexec/omlx-launcher.py"
    cat > "$out/bin/omlx-launcher" <<WRAPPER
    #!/bin/sh
    # Runs the package launcher with the bundle's Python, as the bundle's own omlx-cli does.
    RES="$out/libexec/oMLX.app/Contents/Resources"
    export PYTHONHOME="\$RES/Python/cpython-3.11"
    export PYTHONPATH="\$RES:\$RES/Python/framework-mlx-base/lib/python3.11/site-packages"
    export PYTHONDONTWRITEBYTECODE=1
    exec "\$PYTHONHOME/bin/python3" "$out/libexec/omlx-launcher.py" "\$@"
    WRAPPER
    chmod +x "$out/bin/omlx-launcher"

    runHook postInstall
  '';

  meta = {
    description = "oMLX inference engine, official macOS release, with its package launcher";
    homepage = "https://github.com/jundot/omlx";
    license = lib.licenses.asl20;
    platforms = [ "aarch64-darwin" ];
    mainProgram = "omlx-launcher";
  };
})
