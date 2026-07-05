# build the board (compiles only bakemono-board and its deps from the workspace)
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p bakemono-board

FROM debian:bookworm-slim
# ffmpeg thumbnails the scrape worker's media; gallery-dl (pipx, isolated) does the fetching
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg pipx \
    && rm -rf /var/lib/apt/lists/* \
    && PIPX_HOME=/opt/pipx PIPX_BIN_DIR=/usr/local/bin pipx install gallery-dl \
    && PIPX_HOME=/opt/pipx PIPX_BIN_DIR=/usr/local/bin pipx install yt-dlp
COPY --from=builder /src/target/release/bakemono /usr/local/bin/bakemono
ENV BAKEMONO_BIND=0.0.0.0:3000
# scrape staging + gallery-dl download archive; mount a volume so re-scrapes stay incremental
ENV BAKEMONO_SCRAPE_DIR=/scrape
EXPOSE 3000
ENTRYPOINT ["bakemono"]
