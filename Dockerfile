FROM rust:1.96-slim-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/seadexerr /usr/local/bin/seadexerr

CMD ["/usr/local/bin/seadexerr"]
