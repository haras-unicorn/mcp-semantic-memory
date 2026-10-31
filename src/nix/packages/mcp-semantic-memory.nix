{ inputs, ... }:

{
  self.overlays.overlays.mcp-semantic-memory = {
    flake = true;
    value =
      final: _prev:
      let
        inherit (final) lib;

        root = ../../..;

        workspaceManifest = builtins.fromTOML (builtins.readFile ../../../Cargo.toml);

        manifest = builtins.fromTOML (
          builtins.readFile ../../crates/mcp-semantic-memory-cli/Cargo.toml
        );

        # NOTE: only linux gets the musl target - there is no static darwin
        isLinux = final.stdenv.hostPlatform.isLinux;
        target = "${final.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
        targetUnderscored = builtins.replaceStrings [ "-" ] [ "_" ] target;

        # NOTE: C dependencies (openssl, sqlite, sqlite-vec) must be compiled
        # and linked against musl and not glibc
        musl = final.pkgsStatic.stdenv.cc;
        muslCc = "${musl}/bin/${musl.targetPrefix}cc";

        # NOTE: the TLS is the system's, which is OpenSSL, and a static binary
        # needs it as static libraries built for musl
        openssl = final.pkgsStatic.openssl;

        # NOTE: haraspkgs overlays are not available to downstream overlays
        # so this is what its rust helpers do, using its inputs
        rustBin = inputs.haraspkgs.inputs.rust-overlay.lib.mkRustBin { } final;

        toolchain = rustBin.stable.latest.default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-analyzer"
            # NOTE: rust-analyzer reads the standard library source for its types
            "rust-src"
          ];
          targets = lib.optional isLinux target;
        };

        craneLib = (inputs.haraspkgs.inputs.crane.mkLib final).overrideToolchain (
          _: toolchain
        );

        # NOTE: the migrations are embedded in the binary at build time, so
        # the .sql files have to be in the source along with the cargo files,
        # and so do the .sqlx of the store crates, which the query macros read
        # instead of a database
        src = lib.cleanSourceWith {
          src = lib.fileset.toSource {
            inherit root;
            fileset = lib.fileset.unions [
              (root + "/Cargo.toml")
              (root + "/Cargo.lock")
              ../../crates
            ];
          };
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || (lib.hasSuffix ".sql" path)
            || (lib.hasInfix "/.sqlx/" path);
          name = "source";
        };

        # NOTE: used by the dev shell to write .cargo/config.toml so that
        # cargo works offline
        cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };

        commonArgs = {
          inherit src cargoVendorDir;
          pname = "mcp-semantic-memory";
          inherit (manifest.package) version;
          strictDeps = true;
          cargoExtraArgs = "-p mcp-semantic-memory-cli";
          # NOTE: the tests build a tls client, which needs the system roots
          # and the build sandbox has none
          env = {
            SSL_CERT_FILE = "${final.cacert}/etc/ssl/certs/ca-bundle.crt";
            SQLX_OFFLINE = "true";
          }
          // lib.optionalAttrs isLinux {
            OPENSSL_STATIC = "1";
            OPENSSL_LIB_DIR = "${openssl.out}/lib";
            OPENSSL_INCLUDE_DIR = "${openssl.dev}/include";
            CARGO_BUILD_TARGET = target;
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
            # NOTE: sqlite-vec.c redefines uint8_t and friends from the BSD
            # names u_int8_t and friends, which musl does not have - this
            # makes those typedefs say the same thing twice
            "CFLAGS_${targetUnderscored}" =
              "-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t";
            "CC_${targetUnderscored}" = muslCc;
            "CARGO_TARGET_${lib.toUpper targetUnderscored}_LINKER" = muslCc;
          };
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in
      {
        mcp-semantic-memory = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;

            passthru = {
              inherit toolchain craneLib cargoVendorDir;
            };

            meta = {
              description = workspaceManifest.workspace.package.description;
              license = lib.licenses.mit;
              mainProgram = "mcp-semantic-memory";
            };
          }
        );
      };
  };
}
