# Pinned nixpkgs shared by shell.nix and the Docker build, so the container
# runs exactly what the dev shell provides.
import (fetchTarball {
  url = "https://github.com/NixOS/nixpkgs/archive/nixos-26.05.tar.gz";
}) { config.allowUnfree = true; }
