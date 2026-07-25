{
  description = "octane-agent — an AI coding agent harness for the terminal";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system:
          f (import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          }));
    in
    {
      devShells = forAllSystems (pkgs:
        let
          # Toolchain is pinned by ./rust-toolchain.toml; the exact build is
          # pinned by rust-overlay's entry in flake.lock.
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in
        {
          default = pkgs.mkShell {
            name = "octane-agent";

            packages = [
              toolchain

              # cargo helpers
              pkgs.cargo-nextest
              pkgs.cargo-watch
              pkgs.cargo-deny

              # native build deps
              pkgs.pkg-config
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];

            env = {
              # rust-analyzer needs this to find the stdlib sources.
              RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
              RUST_BACKTRACE = "1";
            };

            shellHook = ''
              echo "octane-agent dev shell — $(rustc --version)"
            '';
          };
        });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
