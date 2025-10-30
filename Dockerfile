FROM rust:1.90.0-slim

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo install --path .

CMD ["seadexerr"]

