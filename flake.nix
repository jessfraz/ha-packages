{
  description = "Packages development environment";

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
    # Shared UI and companion crates must resolve to one tagged Git source.
    kitSources = nixpkgs.lib.unique (
      map (package: package.source or "") (
        builtins.filter
          (package: builtins.elem package.name ["ha-app-ui" "ha-app-kit-ha"])
          (builtins.fromTOML (builtins.readFile ./Cargo.lock)).package
      )
    );
    cargoLockFile =
      assert nixpkgs.lib.assertMsg (
        builtins.length kitSources <= 1
        && builtins.all
          (source: builtins.match "git[+]https://github[.]com/jessfraz/ha-app-kit[?]tag=v[0-9]+[.][0-9]+[.][0-9]+(-[0-9A-Za-z.-]+)?#[0-9a-f]{40}" source != null)
          kitSources
      ) "App Kit crates must use one tagged release from github.com/jessfraz/ha-app-kit.";
      ./Cargo.lock;
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
        cargoLock = {
          lockFile = cargoLockFile;
          allowBuiltinFetchGit = true;
          outputHashes = {
            "sqlx-0.9.0" = "sha256-kLvUfUy6vmL9LYU7nzHv+iB6cIJm2CjDf9Yo6wpWrWg=";
          };
        };
        nativeBuildInputs = [pkgs.cmake pkgs.pkg-config];
        cargoBuildFlags = ["--bin" name];
        cargoTestFlags = ["--test" "cli"];
        # The binary contract tests use loopback HTTP fixtures, never live APIs.
        __darwinAllowLocalNetworking = true;
        meta = {
          inherit (manifest.package) description;
          homepage = manifest.package.repository;
          license = [pkgs.lib.licenses.mit pkgs.lib.licenses.asl20];
          mainProgram = name;
          platforms = systems;
        };
      };
    in rec {
      packages = package "packages";
      ha-packages = package "ha-packages";
      default = packages;
    });
    apps = forAllSystems (system: let
      app = name: {
        type = "app";
        program = "${self.packages.${system}.${name}}/bin/${name}";
      };
    in {
      packages = app "packages";
      ha-packages = app "ha-packages";
      default = app "packages";
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
