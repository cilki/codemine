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
  util-linux
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
# codegraph is missing from some nixpkgs pins (it landed after the 26.05
# branch-off); the runner skips indexing when the CLI is absent, and the
# Dockerfile falls back to the upstream installer.
++ lib.optional (pkgs ? codegraph) codegraph
