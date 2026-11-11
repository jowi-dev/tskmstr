{
  description = "tskmstr - Jira/GitHub ticket CLI + TUI (tm)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "tskmstr";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The work::git suite integration-tests ShellGitOps against real
          # `git init` repos, so the check phase needs git on PATH. The
          # runs::footprint Linux test spawns perl to hold ~200 MB in a
          # child process, so perl must be on PATH too.
          nativeCheckInputs = [ pkgs.git pkgs.perl ];
          # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1,
          # which overrides rusqlite's `bundled` feature: libsqlite3-sys skips
          # its bundled SQLite and links the system one found by pkg-config.
          # Plain `cargo build` outside Nix still uses the bundled copy.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.sqlite ];
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt

            pkgs.gh
          ];
        };
      }
    );
}
