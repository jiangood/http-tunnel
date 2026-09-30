{
  description = "A secure, stable and high-performance HTTP reverse proxy for NAT traversal";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: prev: {
        http-tunnel = final.rustPlatform.buildRustPackage {
          pname = "http-tunnel";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = final.lib.fileset.toSource {
            root = ./.;
            fileset = final.lib.fileset.unions [
              ./src
              ./build.rs
              ./Cargo.toml
              ./Cargo.lock
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ final.pkg-config ];
          buildInputs = [ final.zlib ];
          doCheck = false;
          meta = {
            description = "A secure, stable and high-performance HTTP reverse proxy for NAT traversal";
            homepage = "https://github.com/jiangood/http-tunnel";
            license = final.lib.licenses.asl20;
            mainProgram = "http-tunnel";
          };
        };
      };

      packages = forAllSystems (
        pkgs:
        let
          pkgs' = pkgs.extend self.overlays.default;
        in
        {
          http-tunnel = pkgs'.http-tunnel;
          default = pkgs'.http-tunnel;
        }
      );

      apps = forAllSystems (pkgs: rec {
        http-tunnel = {
          type = "app";
          program = pkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.http-tunnel;
        };
        default = http-tunnel;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.http-tunnel ];
          packages = with pkgs; [
            rustfmt
            clippy
            rust-analyzer
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        http-tunnel = self.packages.${pkgs.stdenv.hostPlatform.system}.http-tunnel;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
