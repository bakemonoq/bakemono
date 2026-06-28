# build the board (compiles only bakemono-board and its deps from the workspace)
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p bakemono-board

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/bakemono-board /usr/local/bin/bakemono-board
# bind all interfaces inside the container; the webtorrent.min.js asset is embedded in the binary
ENV BAKEMONO_BIND=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["bakemono-board"]
