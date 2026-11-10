{
  description = "Stateless MCP server for long-term semantic memory";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";

    haraspkgs.url = "github:haras-unicorn/haraspkgs/v1.5.2";
    haraspkgs.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { haraspkgs, ... }@inputs:
    haraspkgs.lib.makeFlake {
      inherit inputs;
      modules = [ ./src/nix ];
    };
}
