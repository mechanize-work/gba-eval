# Reference-oracle sidecar.
#
#   POST :8001/oracle              — one-shot oracle CLI proxy
#   POST :8001/oracle/session/*    — stateful session API
#
# Sits on the private compose network with the `task` container. The
# agent-side `oracle` thin client (in quickstart/task/oracle-client)
# forwards argv here over HTTP.

# ─── Builder: oracle binary ──────────────────────────────────────────
FROM rust:1.87-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    clang pkg-config zlib1g-dev python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release -p oracle
RUN strip target/release/oracle

# ─── Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

# Oracle binary + the Mesen reference wasm it loads at startup. The
# wasm is the unified mesen_step.wasm that the browser and grader
# consume, so oracle output is byte-identical to the graded reference.
COPY --from=builder --chown=root:root --chmod=0755 \
    /build/target/release/oracle /usr/local/bin/oracle
# COPY --chmod only applies to the FILE. Parent dir would end up 0755
# by default which is fine, but install -d makes the ownership explicit.
RUN install -d -m 0755 /opt/gba-eval
COPY --chown=root:root --chmod=0644 \
    reference/mesen.wasm /opt/gba-eval/mesen.wasm

# HTTP server + entrypoint.
COPY --chown=root:root --chmod=0755 \
    quickstart/services/oracle-server.py /usr/local/bin/oracle-server
COPY --chown=root:root --chmod=0755 \
    quickstart/services/run.sh /usr/local/bin/services-run

# Dedicated uid 5000. Shared group `gba` (gid 3000) matches the task
# image's group so both sides can read+write /task files.
RUN groupadd -g 3000 gba && \
    useradd -r -u 5000 -m -d /home/services -G gba services

USER services
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/services-run"]
