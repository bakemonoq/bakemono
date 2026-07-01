# build the board (compiles only bakemono-board and its deps from the workspace)
FROM rust:1-bookworm AS builder
WORKDIR /src
# librqbit pulls reqwest with native-tls, so the build needs openssl headers + pkg-config
RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p bakemono-board

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/bakemono-board /usr/local/bin/bakemono-board
ENV BAKEMONO_BIND=0.0.0.0:3000
# BT peer port the gateway listens on, so NAT'd seeders can dial in; keep in sync with BAKEMONO_GATEWAY_PORT
ENV BAKEMONO_GATEWAY_PORT=4240
# persistent on-disk cache; mount a volume here so downloads survive container recreation
ENV BAKEMONO_GATEWAY_DIR=/cache
EXPOSE 3000
EXPOSE 4240
EXPOSE 4240/udp
ENTRYPOINT ["bakemono-board"]
