# Static musl build, shipped FROM scratch: the image is the binary and nothing
# else. TLS roots are compiled in (webpki-roots), so no CA bundle is needed.
FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
# Build memory peaks at ~1 GB with 4 jobs and ~650 MB with 1 (the final LTO
# step); lower it on a small VPS.
ARG CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --jobs "$CARGO_BUILD_JOBS" \
    && cp target/release/hevy-mcp /hevy-mcp

FROM scratch
COPY --from=build /hevy-mcp /hevy-mcp
# .env is bind-mounted here and read at start.
WORKDIR /app
ENV HEVY_MCP_BIND=0.0.0.0:8010
EXPOSE 8010
ENTRYPOINT ["/hevy-mcp"]
