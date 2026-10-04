{
  description = "user-service-switcher — Rust dev shell and package";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system: f nixpkgs.legacyPackages.${system}
        );
      # Newest stable toolchain on the nixos-26.05 channel: the `rustChannels`
      # attribute no longer exists in this release, so pick the newest
      # versioned set explicitly.
      rustOf = p: p.rust_1_98.packages.stable;
    in
    {
      packages = forAllSystems (
        p:
        let
          rust = rustOf p;
        in
        {
          default = rust.rustPlatform.buildRustPackage {
            pname = "user-service-switcher";
            version = "0.1.0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
          };
        }
      );

      devShells = forAllSystems (
        p:
        let
          rust = rustOf p;
        in
        {
          default = p.mkShell {
            name = "user-service-switcher";
            packages = [
              rust.rustc
              rust.cargo
              rust.clippy
              rust.rustfmt
              p.rust-analyzer
              p.just # local CI runner (justfile at the repo root)
            ];
          };
        }
      );
    };
}
