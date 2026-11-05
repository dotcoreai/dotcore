{
  pkgs,
  craneLib,
  root,
  extraTestSourceRoots ? [ ],
  testEnvironment ? { },
}:

let
  # Workspace-local build outputs must not become backend derivation inputs.
  isBackendSourcePath =
    path: pkgs.lib.baseNameOf path != ".direnv" && pkgs.lib.baseNameOf path != "target";

  # Non-Rust files the backend embeds into its binaries at compile time: the CLI's coding-agent
  # provider and the hardware simulated inference offers.
  isEmbeddedSource =
    path:
    toString path == toString (root + "/backend/cli/src/code/dotcore-provider.ts")
    || pkgs.lib.hasPrefix (toString (root + "/backend/node_hardware/simulated")) (toString path);

  # The production source contains only Cargo inputs, protocol definitions, and embedded files.
  backendSrc = pkgs.lib.cleanSourceWith {
    src = root;
    filter =
      path: type:
      isBackendSourcePath path
      && (
        craneLib.filterCargoSources path type
        || pkgs.lib.hasSuffix ".proto" path
        || isEmbeddedSource path
      );
  };

  # Workspace tests additionally consume protocol trees, fixtures, the gated tests' machine
  # descriptions, and catalog declarations.
  # Local site state and result symlinks must not affect the test derivation.
  backendTestSrc = pkgs.lib.cleanSourceWith {
    src = root;
    filter =
      path: type:
      let
        pathString = toString path;
        containsAdditionalTestSource =
          source: pathString == toString source || pkgs.lib.hasPrefix "${toString source}/" pathString;
      in
      isBackendSourcePath path
      && pkgs.lib.baseNameOf path != ".dotcore"
      && pkgs.lib.baseNameOf path != "result"
      && (
        craneLib.filterCargoSources path type
        || isEmbeddedSource path
        || pkgs.lib.hasPrefix (toString (root + "/proto")) pathString
        || pkgs.lib.hasPrefix (toString (root + "/backend/fixtures")) pathString
        || pkgs.lib.hasPrefix (toString (root + "/backend/core/tests/gated/machines")) pathString
        || pkgs.lib.hasPrefix (toString (root + "/catalog")) pathString
        || pathString == toString (root + "/backend/.config/nextest.toml")
        || pkgs.lib.any containsAdditionalTestSource extraTestSourceRoots
      );
  };

  backendArgs = {
    pname = "dotcore-backend";
    version = "0.1.0";
    src = backendSrc;
    cargoLock = root + "/backend/Cargo.lock";
    sourceRoot = "source/backend";
    cargoExtraArgs = "--locked --package dotc --bin dotc --package dotcore-launcher --bin dotcore-launcher";
    strictDeps = true;
    nativeBuildInputs = with pkgs; [
      clang
      cmake
      protobuf
    ];
    PROTOC = "${pkgs.protobuf}/bin/protoc";
    PROTOC_INCLUDE = "${pkgs.protobuf}/include";
  };

  cargoArtifacts = craneLib.buildDepsOnly backendArgs;

  backendTestArgs = backendArgs // {
    src = backendTestSrc;
    CARGO_PROFILE = "";
    cargoExtraArgs = "--locked --workspace";
  };

  # Cargo's production and test profiles require separate reusable dependency artifacts.
  backendTestArtifacts = craneLib.buildDepsOnly backendTestArgs;

  caBundlePath = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

  backendTest = craneLib.cargoNextest (
    backendTestArgs
    // {
      cargoArtifacts = backendTestArtifacts;

      postCheck = ''
        # Run doctests separately since nextest doesn't support them yet
        # https://github.com/nextest-rs/nextest/issues/16
        cargo test --locked --workspace --doc
      '';

      # This leaf check emits no archive: crane's target archive is a mtime-selected
      # delta that differs between identical builds, breaking --rebuild determinism.
      doInstallCargoArtifacts = false;

      # Cargo test binaries run in the certificate-less sandbox, so set this unconditionally
      # while eager reqwest native-root clients are constructed.
      SSL_CERT_FILE = caBundlePath;
      RUST_LIB_BACKTRACE = "1";
    }
    // testEnvironment
  );

  backend = craneLib.buildPackage (
    backendArgs
    // {
      inherit cargoArtifacts;

      nativeBuildInputs = backendArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];

      # The installed dotc process gets a default only, so callers can override it.
      postFixup = ''
        wrapProgram $out/bin/dotc \
          --set-default SSL_CERT_FILE "${caBundlePath}"
      '';

      meta = {
        mainProgram = "dotc";
        license = pkgs.lib.licenses.asl20;
      };

      passthru.tests.default = backendTest;

      # Integration tests require fixture and runtime inputs outside the package source.
      doCheck = false;
    }
  );

  dotcDevelopmentCommand = pkgs.writeShellApplication {
    name = "dotc";
    runtimeInputs = [ pkgs.nix ];
    text = ''
      exec nix run .#dotc -- "$@"
    '';
  };

  dotcoreDevelopmentCommand = pkgs.writeShellApplication {
    name = "dotcore";
    runtimeInputs = [ pkgs.nix ];
    text = ''
      exec nix run .#dotcore -- "$@"
    '';
  };
in
{
  package = backend;
  test = backendTest;
  cli = dotcDevelopmentCommand;
  dotcoreCli = dotcoreDevelopmentCommand;
}
