{ selfLib, ... }:

{
  # NOTE: this is a flake check and not one of the dev shell checks, because
  # the dev shell would then depend on the package build, and the package build
  # needs what `dev prepare` writes - so a stale .sqlx would lock `dev prepare`
  # out of the dev shell it runs in
  perSystem =
    { lib, pkgs, ... }:
    let
      mcpMemory = pkgs.${selfLib.overlays.namespace}.mcp-semantic-memory;
    in
    {
      # NOTE: the linux binary must not need anything outside of itself
      checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        static =
          pkgs.runCommand "mcp-semantic-memory-static"
            {
              nativeBuildInputs = [ pkgs.file ];
            }
            ''
              bin="${lib.getExe mcpMemory}"
              echo "checking $bin for static linkage"
              if ! file -b "$bin" | grep -qE "statically linked|static-pie linked"; then
                echo "$bin is not statically linked:" >&2
                file -b "$bin" >&2
                exit 1
              fi
              # NOTE: ldd exits 0 for static-pie, so look at what it prints
              needed="$(${lib.getBin pkgs.glibc}/bin/ldd "$bin" 2>&1 || true)"
              echo "ldd: $needed"
              if echo "$needed" | grep -qE "=>|ld-linux|ld-musl|\.so"; then
                echo "$bin is dynamically linked" >&2
                exit 1
              fi
              touch "$out"
            '';
      };
    };
}
