{
  description = "Development environment for BAG";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/0dd31db7e6dbf9ce05697c4545f6fe01accec994";

    rust-overlay = {
      url = "github:oxalica/rust-overlay/b479967b8ed7aca40ba52cf12f460484c53928a9";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    ...
  }: let
    supportedSystems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forEachSystem = nixpkgs.lib.genAttrs supportedSystems;
  in {
    devShells = forEachSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      rustToolchain = pkgs.rust-bin.stable."1.97.1".default.override {
        extensions = [
          "clippy"
          "rust-src"
          "rustfmt"
        ];
        targets = ["wasm32-unknown-unknown"];
      };
      wasmBindgenCli = pkgs.buildWasmBindgenCli rec {
        src = pkgs.fetchCrate {
          pname = "wasm-bindgen-cli";
          version = "0.2.125";
          hash = "sha256-zRawtjxMOdTMX+mZaiNR3YYfTiZJhf9qj7kXSSeMxrc=";
        };

        cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
          inherit src;
          inherit (src) pname version;
          hash = "sha256-aZCfgR23Qb0Pn4Mm4ToMtuuRQqSJjXCR9li/VvP5CTM=";
        };
      };
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          rustToolchain
          clang
          ffmpeg
          pkg-config
          sqlite
          trunk
          wasmBindgenCli
        ];

        LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
      };
    });
  };
}
