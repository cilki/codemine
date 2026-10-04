#!/bin/sh
# Start CLIProxyAPI next to codemine so the container is self-contained: the
# proxy owns the Claude subscription login and its continuous token refresh,
# and codemine's default proxy settings already point at it. Its config and
# OAuth tokens live under the workspace so a login survives container
# recreation along with everything else on the volume.
set -eu

dir=/workspace/cliproxyapi
mkdir -p "$dir/auth"
if [ ! -f "$dir/config.yaml" ]; then
  cat > "$dir/config.yaml" <<EOF
config-version: 8
server:
  port: 8317
oauth:
  auth-dir: "$dir/auth"
EOF
fi

# Run from its own directory so anything it drops (logs) stays out of the
# workspace root, where codemine owns the layout.
(cd "$dir" && exec cliproxyapi -config "$dir/config.yaml") &

# The workspace default is the volume mount; an explicit --workspace in the
# container arguments still wins because the last flag is the one used.
exec codemine --workspace /workspace "$@"
