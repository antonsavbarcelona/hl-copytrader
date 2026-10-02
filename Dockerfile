# Build, then run the binary alone. The run is kept in Postgres (DATABASE_URL), else in /data.
FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/target/release/hl-copytrader /usr/local/bin/hl-copytrader
COPY start.sh /usr/local/bin/start.sh
# Strip CRLF (a Windows checkout) so the shebang works.
RUN sed -i 's/\r$//' /usr/local/bin/start.sh && chmod +x /usr/local/bin/start.sh
WORKDIR /data
# Settings by env: DATABASE_URL, RUN_ID, API_WEIGHT, EXTRA_ARGS, DATA_DIR (see start.sh).
# Report: docker exec <container> hl-copytrader report --data /data
CMD ["/usr/local/bin/start.sh"]
