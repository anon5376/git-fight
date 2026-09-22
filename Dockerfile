# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
# Debian bookworm Node 18 is enough for Vite 6. Skip Playwright's browser fetch.
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
RUN rustup target add wasm32-unknown-unknown \
    && apt-get update \
    && apt-get install -y --no-install-recommends nodejs npm ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/install-wasm-pack.sh /tmp/install-wasm-pack.sh
RUN sh /tmp/install-wasm-pack.sh
COPY . .
RUN cargo build --release -p git-fight-server
WORKDIR /src/web
RUN npm ci && npm run build

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && groupadd --system --gid 65532 gitfight \
    && useradd --system --uid 65532 --gid gitfight --home-dir /nonexistent \
        --no-create-home --shell /usr/sbin/nologin gitfight \
    && mkdir -p /data \
    && chown gitfight:gitfight /data \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/git-fight-server /usr/local/bin/git-fight-server
COPY --from=build /src/web/dist /app/web
ENV GIT_CONFIG_GLOBAL=/dev/null
ENV GIT_CONFIG_NOSYSTEM=1
EXPOSE 8080
VOLUME ["/data"]
USER gitfight
# Bind is 0.0.0.0; GitHub App mode requires GIT_FIGHT_PUBLIC_URL (a real host, not 0.0.0.0).
CMD ["git-fight-server", "--bind", "0.0.0.0:8080", "--static", "/app/web", "--db", "sqlite:///data/git-fight.db"]
