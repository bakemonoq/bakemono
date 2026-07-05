# build the board (compiles only bakemono-board and its deps from the workspace)
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p bakemono-board

FROM debian:bookworm-slim
# ffmpeg thumbnails the scrape worker's media; gallery-dl (pipx, isolated) does the fetching.
# gallery-dl-fanbox is the RavenYin curl_cffi fork: its firefox TLS impersonation is the only way
# past pixiv's Cloudflare on api.fanbox.cc. git is needed to pip-clone the fork
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg pipx git \
    && rm -rf /var/lib/apt/lists/* \
    && export PIPX_HOME=/opt/pipx PIPX_BIN_DIR=/usr/local/bin \
    && pipx install gallery-dl \
    && pipx install yt-dlp \
    && pipx install --suffix=-fanbox "git+https://codeberg.org/RavenYin/gallery-dl.git@pr/fanbox-curl-cffi" \
    && pipx inject gallery-dl-fanbox curl_cffi
COPY --from=builder /src/target/release/bakemono /usr/local/bin/bakemono
ENV BAKEMONO_BIND=0.0.0.0:3000
# scrape staging + gallery-dl download archive; mount a volume so re-scrapes stay incremental
ENV BAKEMONO_SCRAPE_DIR=/scrape
EXPOSE 3000
ENTRYPOINT ["bakemono"]
