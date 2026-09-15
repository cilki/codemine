FROM nixos/nix:latest AS nix-base

# Needed for 'nix profile'
COPY ./etc /etc

# Build codemine inside the pinned dev shell so the binary links against the
# same glibc the runtime stage installs.
FROM nix-base AS build
WORKDIR /build
COPY nix/ ./nix/
COPY shell.nix ./
RUN nix-shell shell.nix --run true
COPY . .
RUN nix-shell shell.nix --run "cargo build --release" \
  && install -Dm755 target/release/codemine /out/bin/codemine

FROM nix-base

ENV TZ=America/Chicago

COPY nix/ /work/nix/
RUN nix-env -if /work/nix/profile.nix \
  && nix-collect-garbage -d \
  && rm -rf /root/.cache/nix /nix/var/log/nix /work

# Load the Claude OAuth plugin from the nix store so opencode never has to
# fetch it from npm at startup.
RUN pkg=/root/.nix-profile/lib/node_modules/opencode-claude-auth \
  && main=$(jq -r '.main // "index.js"' "$pkg/package.json") \
  && test -f "$pkg/$main" \
  && mkdir -p /root/.config/opencode/plugins \
  && ln -s "$pkg/$main" /root/.config/opencode/plugins/opencode-claude-auth.js

COPY commands/ /root/.config/opencode/commands/
COPY skills/ /root/.config/opencode/skills/
COPY --from=build /out/bin/codemine /usr/local/bin/codemine

ENV PATH=/root/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH
RUN mkdir -p /workspace
WORKDIR /workspace

ENTRYPOINT [ "/usr/local/bin/codemine" ]
