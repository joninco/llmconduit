# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

FROM gcr.io/distroless/cc-debian12:nonroot

ENV HOME=/home/nonroot \
    XDG_CONFIG_HOME=/home/nonroot/.config \
    RUST_LOG=info

COPY --from=builder /app/target/release/resp2chat /usr/local/bin/resp2chat

EXPOSE 4000

ENTRYPOINT ["/usr/local/bin/resp2chat"]
CMD ["start"]
