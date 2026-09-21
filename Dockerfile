# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
RUN rustup target add wasm32-unknown-unknown \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
RUN curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
COPY . .
RUN cargo build --release -p git-fight-server
WORKDIR /src/web
RUN npm ci && npm run build

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/git-fight-server /usr/local/bin/git-fight-server
COPY --from=build /src/web/dist /app/web
ENV GIT_CONFIG_GLOBAL=/dev/null
ENV GIT_CONFIG_NOSYSTEM=1
EXPOSE 8080
VOLUME ["/data"]
# Bind is 0.0.0.0; GitHub App mode requires GIT_FIGHT_PUBLIC_URL (a real host, not 0.0.0.0).
CMD ["git-fight-server", "--bind", "0.0.0.0:8080", "--static", "/app/web", "--db", "sqlite:///data/git-fight.db"]
