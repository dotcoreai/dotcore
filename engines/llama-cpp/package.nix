{
  lib,
  stdenvNoCC,
  fetchurl,
}:

# Packages the upstream release tarball untouched: rewriting Mach-O files would
# invalidate their arm64 signatures, so the fixup phase is disabled.
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "llama-cpp";
  version = "b12774";

  src = fetchurl {
    url = "https://github.com/ggml-org/llama.cpp/releases/download/${finalAttrs.version}/llama-${finalAttrs.version}-bin-macos-arm64.tar.gz";
    hash = "sha256-cDEcR1HtMiHmP0d2kyfdCI10APdtz7gcZuyEPbFlgU0=";
  };

  dontConfigure = true;
  dontBuild = true;
  dontFixup = true;

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/libexec/llama-cpp" "$out/bin"
    cp -R . "$out/libexec/llama-cpp/"

    # Dylibs stay beside the tools so @loader_path resolves; only executables are linked.
    for tool in "$out"/libexec/llama-cpp/*; do
      case "$tool" in
        *.dylib) continue ;;
      esac
      if [ -f "$tool" ] && [ -x "$tool" ]; then
        ln -s "$tool" "$out/bin/$(basename "$tool")"
      fi
    done

    runHook postInstall
  '';

  meta = {
    description = "llama.cpp inference engine, official macOS arm64 release";
    homepage = "https://github.com/ggml-org/llama.cpp";
    license = lib.licenses.mit;
    platforms = [ "aarch64-darwin" ];
    mainProgram = "llama-server";
  };
})
