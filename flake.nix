{
  description = "Shipments development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    ...
  }: let
    systems = ["aarch64-darwin" "aarch64-linux" "x86_64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    packages = forAllSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
      manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      package = name: rustPlatform.buildRustPackage {
        pname = name;
        inherit (manifest.package) version;
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml ./Cargo.lock ./src ./migrations ./web ./tests ./openapi
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [pkgs.cmake pkgs.pkg-config];
        cargoBuildFlags = ["--bin" name];
        cargoTestFlags = ["--test" "cli"];
        # The binary contract tests use loopback HTTP fixtures, never live APIs.
        __darwinAllowLocalNetworking = true;
        meta = {
          inherit (manifest.package) description;
          homepage = manifest.package.repository;
          license = pkgs.lib.licenses.mit;
          mainProgram = name;
          platforms = systems;
        };
      };
    in rec {
      shipments = package "shipments";
      ha-shipments = package "ha-shipments";
      default = shipments;
    });
    apps = forAllSystems (system: let
      app = name: {
        type = "app";
        program = "${self.packages.${system}.${name}}/bin/${name}";
      };
    in {
      shipments = app "shipments";
      ha-shipments = app "ha-shipments";
      default = app "shipments";
    });
    devShells = forAllSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
          python314
          uv
          cmake
          pkg-config
          sqlite
          jq
          actionlint
          shellcheck
          cargo-deny
        ];
      };
    });
  };
}
