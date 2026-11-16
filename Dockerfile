# Multi-stage build. Targets: dev (compiles and runs from mounted sources) and prod (default, last).
ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-slim-trixie AS base
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Dev: the compose file mounts crates/ and Cargo.* over the copied sources and keeps target/ in a volume.
FROM base AS dev
CMD ["cargo", "run", "--locked", "-p", "ledger-api"]

FROM base AS build
RUN cargo build --release --locked -p ledger-api

# Prod: only the binary, as an unprivileged user.
FROM debian:trixie-slim AS prod
RUN useradd --system --uid 10001 --no-create-home ledger
COPY --from=build /app/target/release/ledger-api /usr/local/bin/ledger-api
USER ledger
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["ledger-api"]
CMD ["serve"]
