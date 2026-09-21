# Pinned nixpkgs shared by shell.nix and the Docker build, so the container
# runs exactly what the dev shell provides.
import (fetchTarball {
  url = "https://github.com/NixOS/nixpkgs/archive/nixos-26.05.tar.gz";
}) {
  config.allowUnfree = true;
  overlays = [
    (final: prev: {
      # nixpkgs is stuck on 0.7.3, which predates Anthropic billing
      # third-party OAuth apps to extra usage instead of the subscription;
      # 2.2.0 presents requests as a Claude Code session again
      # (opencode-claude-auth#145). Sources changed to a pnpm build the
      # nixpkgs derivation can't drive, but the published npm tarball is a
      # dependency-free bundle, so it only needs unpacking.
      opencode-claude-auth = final.stdenvNoCC.mkDerivation (finalAttrs: {
        pname = "opencode-claude-auth";
        version = "2.2.0";
        src = final.fetchurl {
          url =
            "https://registry.npmjs.org/opencode-claude-auth/-/opencode-claude-auth-${finalAttrs.version}.tgz";
          hash =
            "sha512-EYU6hbP9edKQABn5zKMXPdxT5KZLf/0qII7AgahCW2w/FGgJnSE5bbWU5bTGmlHZTAJXixdy94/3WpBgIlHMaA==";
        };
        installPhase = ''
          mkdir -p $out/lib/node_modules/opencode-claude-auth
          cp -r . $out/lib/node_modules/opencode-claude-auth
        '';
        inherit (prev.opencode-claude-auth) meta;
      });
    })
  ];
}
