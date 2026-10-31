{
  self.overlays.overlays.docs = {
    flake = true;
    value = final: _prev: {
      docs =
        final.runCommand "mcp-semantic-memory-docs"
          {
            src = final.lib.fileset.toSource {
              root = ../../..;
              fileset = ../../../docs;
            };
            nativeBuildInputs = [ final.mdbook ];
          }
          ''
            mdbook build -d "$out" "$src/docs"
          '';
    };
  };
}
