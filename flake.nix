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
        # Define the default package with dbus support and other build inputs
        defaultPackage = naersk-lib.buildPackage {
          src = ./.;
          nativeBuildInputs = [ pkgs.dbus pkgs.pkg-config ];
          buildInputs = [ pkgs.dbus ];
        };

        # Development shell definition
        devShell = with pkgs; mkShell {
          buildInputs = [ cargo rustc rustfmt pre-commit rustPackages.clippy dbus sqlite ];
          nativeBuildInputs = [ pkg-config ];
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };

        # Application definition
        apps.default = {
          type = "app";
          program = "${self.defaultPackage.${system}}/bin/btle-adv-logger";
        };
      }
    );
}
