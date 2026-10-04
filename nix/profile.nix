# The runtime environment the Docker image installs with `nix-env -if`.
let pkgs = import ./nixpkgs.nix;
in pkgs.buildEnv {
  name = "codemine-runtime";
  paths = import ./runtime.nix { inherit pkgs; };
  # The Docker base image's profile already holds bash-interactive, which
  # collides with our bash on shared locale files; a priority below the
  # default 5 lets nix-env resolve the collision in our favor.
  meta.priority = 4;
}
