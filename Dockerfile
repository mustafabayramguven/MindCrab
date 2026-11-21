# syntax=docker/dockerfile:1
# ---- build: static musl binary ------------------------------------------------
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cache dependencies separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/mindcrab* target/release/mindcrab*
COPY src ./src
COPY web ./web
RUN cargo build --release --locked && strip target/release/mindcrab

# ---- runtime ----------------------------------------------------------------
FROM alpine:3
RUN apk add --no-cache ca-certificates tzdata \
    && addgroup -g 1000 mindcrab && adduser -D -u 1000 -G mindcrab mindcrab \
    && mkdir -p /data/vault && chown -R mindcrab:mindcrab /data
COPY --from=build /src/target/release/mindcrab /usr/local/bin/mindcrab
USER mindcrab
ENV MINDCRAB_VAULT=/data/vault \
    MINDCRAB_HOST=0.0.0.0 \
    MINDCRAB_PORT=3436
EXPOSE 3436
VOLUME ["/data/vault"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
    CMD wget -qO- http://127.0.0.1:3436/api/health >/dev/null || exit 1
ENTRYPOINT ["mindcrab"]
CMD ["serve"]
