# Use Rust official image as builder
FROM rustlang/rust:nightly-bullseye-slim as builder
WORKDIR /
# Copy Cargo files and compile dependencies
COPY . ./
RUN apt-get update
RUN apt-get -y install pkg-config libssl-dev
RUN rustup update
RUN cargo build --release

FROM debian:bullseye-slim
RUN apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Copy binary from builder stage
COPY --from=builder /target/release/bing-wallpaper-service /bing-wallpaper-service
EXPOSE 3000/tcp
# Run the application
CMD ["/bing-wallpaper-service"]
