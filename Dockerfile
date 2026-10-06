# syntax=docker/dockerfile:1

# Build stage. BoringSSL (through btls-sys) needs cmake and clang; bindgen needs libclang.
FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang libclang-dev perl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Only what the build reads, so documentation edits do not invalidate the cached build.
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# Patched crates referenced by [patch.crates-io] in Cargo.toml.
COPY vendor vendor
COPY ui/dist ui/dist
# On small machines pass --build-arg BUILD_JOBS=1; the final link needs about 2 GB of memory.
ARG BUILD_JOBS
RUN cargo build --release --locked -p cliproxy ${BUILD_JOBS:+--jobs "$BUILD_JOBS"} \
 && install -m 0755 target/release/cliproxy /usr/local/bin/cliproxy

# Runtime stage. The binary links glibc and libstdc++, so a slim Debian base is the
# smallest image that runs it unchanged.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 cliproxy \
 && useradd --system --uid 10001 --gid 10001 --home-dir /data --shell /usr/sbin/nologin cliproxy \
 && mkdir -p /data \
 && chown cliproxy:cliproxy /data
COPY --from=build /usr/local/bin/cliproxy /usr/local/bin/cliproxy
USER cliproxy
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8317
ENTRYPOINT ["cliproxy"]
CMD ["--config", "/data/config.yaml"]
