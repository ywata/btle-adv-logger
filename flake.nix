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
        packages.default = naersk-lib.buildPackage {
          src = ./.;
          nativeBuildInputs = [ pkgs.dbus pkgs.pkg-config pkgs.sqlite];
          buildInputs = [ pkgs.dbus ];
        };

        # Development shell definition
        devShells.default = pkgs.mkShell {
            buildInputs = [pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.pre-commit pkgs.clippy pkgs.dbus pkgs.sqlite];
            nativeBuildInputs = [pkgs.pkg-config];
        };


        # Application definition
        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/btle-adv-logger";
          meta = {
            description = "Bluetooth Low Energy Advertisement Logger";
            mainProgram = "btle-adv-logger";
          };
        };
        checks = {
        # A derivation that builds and tests your Rust project
          my-tests = naersk-lib.buildPackage {
            src = ./.;
            doCheck = true;    # Enables `cargo test`
            # Optional: specify test options
            # cargoTestOptions = [ "--all-features" ];
            nativeBuildInputs = [ pkgs.dbus pkgs.pkg-config pkgs.sqlite];
            buildInputs = [ pkgs.dbus pkgs.sqlite ];
          };
        };
      });
}
