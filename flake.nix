{
  description = "A friendly serial terminal application.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [rust-overlay.overlays.default];
        };
      in {
        packages = rec {
          yap = pkgs.callPackage ./default.nix {};
          default = yap;
        };

        devShells.default = pkgs.mkShell rec {
          nativeBuildInputs = with pkgs; [
            rust-bin.stable.latest.default
            pkg-config
            udev
            openssl
            rustup # for jetbrains IDE
            rust-analyzer
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath nativeBuildInputs;
        };
      }
    );
}
