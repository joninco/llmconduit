# syntax=docker/dockerfile:1

FROM node:22-bookworm-slim AS dashboard-builder

WORKDIR /app/dashboard-frontend
COPY dashboard-frontend/package.json dashboard-frontend/package-lock.json ./
RUN npm ci --no-audit --no-fund

COPY dashboard-frontend/ ./
RUN npm run build

FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY src ./src
COPY benches ./benches
COPY --from=dashboard-builder /app/dashboard-frontend/dist /app/dashboard-dist

RUN LLMCONDUIT_DASHBOARD_DIST=/app/dashboard-dist cargo build --locked --release

FROM gcr.io/distroless/cc-debian12:nonroot

ENV HOME=/home/nonroot \
    XDG_CONFIG_HOME=/home/nonroot/.config \
    LLMCONDUIT_BIND_ADDR=0.0.0.0:4000 \
    RUST_LOG=info

COPY --from=builder /app/target/release/llmconduit /usr/local/bin/llmconduit

EXPOSE 4000

ENTRYPOINT ["/usr/local/bin/llmconduit"]
CMD ["start"]
