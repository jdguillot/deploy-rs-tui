{
  description = "deploy-rs-tui — a terminal UI wrapper for serokell/deploy-rs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    deploy-rs.url = "github:serokell/deploy-rs";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      deploy-rs,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "deploy-rs-tui-dev";

          packages = with pkgs; [
            # Rust toolchain
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer

            # Build tools
            pkg-config

            # Runtime tools the TUI shells out to
            deploy-rs.packages.${system}.deploy-rs
            nix
            openssh
          ];

          # OpenSSL is unused at the moment but commonly needed once HTTPS
          # crates are added.
          buildInputs = with pkgs; [ openssl ];

          RUST_BACKTRACE = "1";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "deploy-rs-tui";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.makeWrapper
          ];
          buildInputs = [ pkgs.openssl ];

          # The TUI shells out to these at runtime.
          postInstall = ''
            wrapProgram $out/bin/deploy-rs-tui \
              --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  deploy-rs.packages.${system}.deploy-rs
                  pkgs.nix
                  pkgs.openssh
                ]
              }
          '';
        };
      }
    );
}
