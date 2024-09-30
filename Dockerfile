# Stage 1: Build the Rust project
FROM rust:1.70 as builder

WORKDIR /usr/src/app

# Copy the entire project into the container
COPY . .

# Build the Rust project in release mode for optimized binary
RUN cargo build --release

# Stage 2: Create the final minimal image with only necessary dependencies
FROM debian:bullseye-slim

# Install necessary shared libraries and wget
RUN apt-get update && apt-get install -y \
    libssl1.1 \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Download and install kubectl
RUN KUBECTL_VERSION=$(wget -qO- https://dl.k8s.io/release/stable.txt) \
    && wget "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" \
    && chmod +x kubectl \
    && mv kubectl /usr/local/bin/kubectl

# Copy the Rust binary from the builder stage
COPY --from=builder /usr/src/app/target/release/operator /usr/local/bin/operator

# Copy Kubernetes manifests into the container
COPY k8s/ /app/k8s/

# Set the working directory to where the manifests are
WORKDIR /app

# Set the entry point to the compiled Rust binary
ENTRYPOINT ["/usr/local/bin/operator"]
