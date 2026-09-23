# One image, three binaries: the invoicing service, the mock PSP, and the
# webhook sink. docker-compose.yml picks which one each container runs.

FROM rust:1.89-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts keep the registry and target dir between builds, so a code
# change does not recompile every dependency.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bins \
    && mkdir /out \
    && cp target/release/invoicing target/release/mock-psp target/release/webhook-sink /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 app
COPY --from=build /out/ /usr/local/bin/
USER app
CMD ["invoicing"]
