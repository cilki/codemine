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
# codegraph is missing from some nixpkgs pins (it landed after the 26.05
# branch-off); the runner skips indexing when the CLI is absent, and the
# Dockerfile falls back to the upstream installer.
++ lib.optional (pkgs ? codegraph) codegraph
# rtk is missing from some nixpkgs pins too; the agent's commands run
# unrewritten when the CLI is absent, and the Dockerfile falls back to the
# upstream installer.
++ lib.optional (pkgs ? rtk) rtk
# cliproxyapi owns the Claude subscription login and its continuous token
# refresh; codemine and opencode are only clients. It hasn't reached the
# 26.05 pin yet, so pull it from nixos-unstable until it lands.
++ [
  (if pkgs ? cliproxyapi then
    cliproxyapi
  else
    (import (builtins.fetchTarball {
      url = "https://github.com/NixOS/nixpkgs/archive/nixos-unstable.tar.gz";
    }) { }).cliproxyapi)
]
