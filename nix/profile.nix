# The runtime environment the Docker image installs with `nix-env -if`.
let pkgs = import ./nixpkgs.nix;
in pkgs.buildEnv {
  name = "codemine-runtime";
  paths = import ./runtime.nix { inherit pkgs; };
}
