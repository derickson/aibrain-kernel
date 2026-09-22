# The Rust service for the compose `full` profile: a host where the vault
# lives in the container's own filesystem, so FSEvents forwarding does not
# matter. In dev on a Mac, run the binary natively instead (see dev.sh).
FROM rust:1.82-bookworm AS build
WORKDIR /src
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/aibrain-core ./aibrain-core
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/aibrain-core /usr/local/bin/aibrain-core
# config.json is mounted at /config; the vaults it names are mounted read-only
# at /vaults, so the paths inside it must be written as /vaults/<name>.
ENV AIBRAIN_HOME=/config
EXPOSE 8781
ENTRYPOINT ["aibrain-core"]
CMD ["serve", "--watch"]
