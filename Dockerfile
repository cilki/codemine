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

COPY --from=build /out/bin/codemine /usr/local/bin/codemine
COPY --chmod=755 entrypoint.sh /usr/local/bin/entrypoint

ENV PATH=/root/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH

# codegraph comes from the nix profile (runtime.nix pulls it from
# nixos-unstable when the pin lacks it); the upstream installer's vendored
# Node runtime doesn't run on this FHS-less base image.
RUN codegraph --version

RUN mkdir -p /workspace
WORKDIR /workspace

# All durable state in one place: codemine's settings and clones, plus
# CLIProxyAPI's config and OAuth tokens (the entrypoint keeps both under
# /workspace). Declared so even a bare `docker run` lands it on a volume.
VOLUME /workspace

# Default port for the always-on web UI (--listen), plus the OAuth callback
# that only answers while `cliproxyapi -claude-login` is running.
EXPOSE 8080 54545

# Starts CLIProxyAPI alongside codemine; arguments go to codemine.
ENTRYPOINT [ "/usr/local/bin/entrypoint" ]
