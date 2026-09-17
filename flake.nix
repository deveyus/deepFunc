{
  description = "keyServ — secret injection daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      # steam-run is unfreeRedistributable (like furnace/shell.nix allowUnfree).
      # Only steam needs it; allow unfree for the shell.
      pkgs = import nixpkgs {
        inherit system;
        config.allowUnfree = true;
      };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          rustfmt
          clippy
          rust-analyzer
          gcc
          git
          why3
          z3
          # Creusot provers beyond why3/z3 (furnace ships cvc5; creusot setup also wants alt-ergo/cvc4)
          alt-ergo
          cvc5
          # coverage + fuzz + bench (cf. furnace/shell.nix, astarte/shell.nix)
          cargo-llvm-cov
          cargo-fuzz
          # iai-callgrind runs the bench under valgrind's callgrind
          valgrind
          # nix's rustc ships without the llvm-tools component; rustc 1.97
          # is built with LLVM 21.1, so point cargo-llvm-cov at llvm_21.
          pkgs.llvm_21
          # Kani: CBMC is the model-checking backend; Kani hardcodes
          # an FHS layout (glibc, libstdc++ paths) that does not match
          # the bare NixOS stub-ld. Even a local `cargo install` of
          # kani-verifier hits /lib64/ld-linux-x86-64.so.2 → stub-ld
          # (https://nix.dev/permalink/stub-ld). furnace fixes this by
          # running Kani inside steam-run's FHS bubblewrap (see
          # furnace/shell.nix: steam-run + verify-track-b.sh). We mirror
          # that here — do not try to patchelf the cargo-installed
          # binaries in place; use the FHS wrapper.
          cbmc
          steam-run
          # toolchain manager for creusot's nightly (furnace uses rustup; flake pins nixpkgs rustc)
          rustup
          # supply chain
          cargo-audit
          cargo-deny
          # fast feedback
          cargo-nextest
          cargo-hack
          typos
          taplo
          nixfmt-rfc-style
          statix
          deadnix
        ];

        # rust-src + LLVM tools for cargo-llvm-cov
        RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
        LLVM_COV = "${pkgs.llvm_21}/bin/llvm-cov";
        LLVM_PROFDATA = "${pkgs.llvm_21}/bin/llvm-profdata";

        # cargo-installed tooling (kani, creusot, iai-callgrind-runner)
        shellHook = ''
          export PATH="$HOME/.cargo/bin:$PATH"
        '';
      };
    };
}
