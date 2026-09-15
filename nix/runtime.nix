# Everything available to the agent at runtime, shared between shell.nix and
# the Docker image (via profile.nix).
{ pkgs }:

with pkgs;
[
  bash
  coreutils
  gnugrep
  gnused
  findutils
  git
  jq
  curl
  ripgrep
  tea
  gh
  glab
  opencode
  opencode-claude-auth
  cargo
  rustc
  clippy
  rustfmt
  gnumake
]
