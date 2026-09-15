{ pkgs ? import ./nix/nixpkgs.nix }:

with pkgs;

mkShell {
  nativeBuildInputs = [ cargo rustc rust-analyzer rustfmt clippy ];
  buildInputs = import ./nix/runtime.nix { inherit pkgs; };
}
