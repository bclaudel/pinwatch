{
  description = "Pinned Linux Rust development environment for Pinwatch";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };
  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in {
        devShells.default = pkgs.mkShell {
          # LSP comes from the SAME toolchain, not a host binary or a newer nixpkgs Rust.
          packages = [
            (rustToolchain.override {
              extensions =
                (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.components
                ++ [ "rust-analyzer" ];
            })
          ];
        };
      });
}
