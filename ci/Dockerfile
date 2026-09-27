# The runner image: the Sandbox runtime plus the toolchains pipelines expect.
# The base tag must match the @cloudflare/sandbox version in package.json
# exactly (both on the @next line).
FROM docker.io/cloudflare/sandbox:0.13.0-next.776.1

# Base already has bash, git, curl, tar, node, npm, and bun. Add a C
# toolchain, and clang/llvm-ar for crates that compile C to wasm.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential clang llvm pkg-config ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Rust, with the wasm target ripgit itself builds for.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
 && rustup target add wasm32-unknown-unknown \
 && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

# The @ripgit/ci SDK and plan evaluator (see sdk/).
COPY sdk /opt/ripgit-ci/sdk
