# syntax=docker/dockerfile:1
# VerbaRelay server image. The client is not containerised; it needs direct GPU access.
#
#   docker build -t verbarelay-server .
#   cp config/server.example.toml config/server.toml   # then edit it
#   docker run -d -p 8443:8443 -v ./config:/etc/verbarelay:ro -v verbarelay-data:/var/lib/verbarelay verbarelay-server

FROM rust:1.98-slim-trixie AS build
WORKDIR /src
# Cargo needs every workspace member's manifest, so the whole workspace is copied
# (see .dockerignore); only the server is built.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p verbarelay-server \
    && cp target/release/verbarelay-server /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 verbarelay \
    && useradd --system --uid 10001 --gid verbarelay --home-dir /var/lib/verbarelay --shell /usr/sbin/nologin verbarelay \
    && install -d -o verbarelay -g verbarelay -m 750 /var/lib/verbarelay /etc/verbarelay
COPY --from=build /usr/local/bin/verbarelay-server /usr/local/bin/verbarelay-server

ENV VERBARELAY_CONFIG=/etc/verbarelay/server.toml
USER verbarelay
VOLUME ["/var/lib/verbarelay"]
EXPOSE 8443
ENTRYPOINT ["verbarelay-server"]
CMD ["serve"]
