FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# cache dependencies separately from the source
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM alpine:3
RUN apk add --no-cache ffmpeg ca-certificates \
    && adduser -D -H -u 10001 proxy
COPY --from=build /src/target/release/tvp-vod-proxy /usr/local/bin/tvp-vod-proxy
USER proxy
EXPOSE 8080
HEALTHCHECK --interval=1m --timeout=5s CMD wget -qO- "http://127.0.0.1:${PORT:-8080}/healthz" || exit 1
ENTRYPOINT ["tvp-vod-proxy"]
CMD ["serve"]
