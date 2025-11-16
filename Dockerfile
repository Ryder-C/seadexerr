FROM rust:1.91.1-slim

WORKDIR /app
COPY Cargo.toml Cargo.lock /app/
COPY src /app/src

RUN cargo install --path .

CMD ["seadexerr"]
