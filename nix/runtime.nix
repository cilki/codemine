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
  cargo
  rustc
  clippy
  rustfmt
  gnumake
]
# Some packages landed after the 26.05 branch-off and are missing from the
# pin; pull those from nixos-unstable until they reach a stable release.
++ (let
  fromUnstable = name:
    if pkgs ? ${name} then
      pkgs.${name}
    else
      (import (builtins.fetchTarball {
        url = "https://github.com/NixOS/nixpkgs/archive/nixos-unstable.tar.gz";
      }) { }).${name};
in [
  # codegraph indexes the workspace clones and serves the codegraph_explore
  # MCP tool to opencode. The upstream installer's vendored Node runtime
  # expects an FHS dynamic loader the nix base image doesn't have, so it must
  # come from nixpkgs.
  (fromUnstable "codegraph")
  # cliproxyapi owns the Claude subscription login and its continuous token
  # refresh; codemine and opencode are only clients.
  (fromUnstable "cliproxyapi")
])
# rtk is missing from some nixpkgs pins too; the agent's commands run
# unrewritten when the CLI is absent, and the Dockerfile falls back to the
# upstream installer.
++ lib.optional (pkgs ? rtk) rtk
