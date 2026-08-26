# The `ann-benchmarks` image for InlaySQL.
#
# Built by `ann-benchmarks`' `install.py` as `ann-benchmarks-inlaysql`, from the
# base `ann-benchmarks` image. Everything InlaySQL needs is inside: the Rust
# toolchain to build the `inlaysql` binary, and the MySQL client the adapter
# reaches it with. There is no server to start here — `module.py` spawns
# `inlaysql serve --mysql` itself on a port the OS picks, so the container has
# no daemon and no fixed port to collide with.
FROM ann-benchmarks

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl git pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir mysql-connector-python

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain 1.89

# Pin the engine by revision so a rebuilt image measures the same engine. Bump
# it deliberately; `main` would make two runs of "the same" image incomparable.
ARG INLAYSQL_REV=main
RUN git clone https://github.com/inlaySQL/inlaysql /opt/inlaysql \
    && cd /opt/inlaysql \
    && git checkout "$INLAYSQL_REV" \
    && cargo build --release -p inlaysql-mcp --bin inlaysql \
    && cp target/release/inlaysql /usr/local/bin/inlaysql \
    && cargo clean

ENV INLAYSQL_BIN=/usr/local/bin/inlaysql
