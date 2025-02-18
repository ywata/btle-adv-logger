{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          # Include your project directory
          src = ./.;

          # Add dbus and pkg-config as dependencies
          nativeBuildInputs = [ pkgs.dbus pkgs.pkg-config ];
          buildInputs = [ pkgs.dbus ];
        };

        # Development shell
        devShell = with pkgs; mkShell {
          buildInputs = [ cargo rustc rustfmt pre-commit rustPackages.clippy dbus ];
          nativeBuildInputs = [ pkg-config ];
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };
      }
    );
}