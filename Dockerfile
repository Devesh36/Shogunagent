# ShogunAgent sidecar image: the Memory API server (`shogun-api`, REST) and the MCP server
# (`shogun-mcp`, stdio) built from this workspace. SQLCipher and OpenSSL are vendored by the
# rusqlite feature set, so the runtime image needs only a libc.
#
#   docker build -t shogunagent .
#   docker run -e SHOGUN_DB_PATH=/data/memory.db -e SHOGUN_DB_KEY=<hex> -e SHOGUN_API_TOKEN=... \
#     -v shogunagent-data:/data -p 7464:7464 shogunagent

FROM rust:1-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends clang pkg-config && rm -rf /var/lib/apt/lists/*
COPY . .
ENV CARGO_INCREMENTAL=0
RUN cargo build --release --locked -p shogun-core --features daemon-server --bin shogun-api --bin shogun-mcp \
 && cargo build --release --locked -p shogun-cli --bin shogun

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --uid 10001 shogun && mkdir -p /data && chown shogun:shogun /data
COPY --from=build /src/target/release/shogun-api /usr/local/bin/shogun-api
COPY --from=build /src/target/release/shogun-mcp /usr/local/bin/shogun-mcp
COPY --from=build /src/target/release/shogun /usr/local/bin/shogun
USER shogun
WORKDIR /data
ENV SHOGUN_DB_PATH=/data/memory.db
ENV SHOGUN_API_PORT=7464
EXPOSE 7464
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/shogun-api"]
