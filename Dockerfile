# Builds the Leptos SSR server (which also serves the keeper API) for Fly.io.
FROM rust:1-bookworm AS build
RUN rustup target add wasm32-unknown-unknown \
 && cargo install cargo-binstall --locked \
 && cargo binstall -y cargo-leptos
WORKDIR /app
COPY . .
RUN cargo leptos build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/target/server/trimtab /app/trimtab
COPY --from=build /app/target/site /app/site
ENV LEPTOS_SITE_ROOT=site LEPTOS_SITE_ADDR=0.0.0.0:8080 LEPTOS_ENV=PROD RUST_LOG=info
EXPOSE 8080
CMD ["/app/trimtab"]
