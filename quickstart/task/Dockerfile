# Task container — the environment the benchmark's agents ran in.
#
# Toolchain (Rust + wasm target + wasmtime + clang + common inspection
# tools), spec, dev ROMs, oracle HTTP client. Agent runs as uid 1000.
#
# Build context MUST be the repo root, so the Dockerfile can reach
# corpus/roms/, spec/, etc. The compose file handles this.
#
# The actual oracle reference emulator lives in the services sidecar;
# this image only ships the oracle HTTP client (a Python script) along
# with the toolchain the agent uses to build its candidate wasm.
#
# Build:  docker compose build task    (or docker compose up -d)
# Run:    (via docker-compose.yml; long-lived idle container,
#          ./shell.sh drops you inside)

FROM rust:1.87-bookworm

RUN rustup target add wasm32-unknown-unknown

# Common tools + wasm linker. xxd/file/jq are inspection tools the
# agents reach for constantly.
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 python3-pip python3-numpy python3-scipy \
    clang lld cmake pkg-config tini \
    xxd file jq ca-certificates curl xz-utils \
    binutils-arm-none-eabi python3-capstone \
    && rm -rf /var/lib/apt/lists/*

# wasmtime — the grader loads wasm via wasmtime, so agents need it
# to validate locally. Pinned release tarball.
ARG WASMTIME_VERSION=v26.0.1
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) target=x86_64-linux ;; \
      arm64) target=aarch64-linux ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    url="https://github.com/bytecodealliance/wasmtime/releases/download/${WASMTIME_VERSION}/wasmtime-${WASMTIME_VERSION}-${target}.tar.xz"; \
    curl -fsSL "$url" -o /tmp/wasmtime.tar.xz; \
    tar -xJf /tmp/wasmtime.tar.xz -C /tmp; \
    install -m 0755 "/tmp/wasmtime-${WASMTIME_VERSION}-${target}/wasmtime" /usr/local/bin/wasmtime; \
    rm -rf /tmp/wasmtime.tar.xz "/tmp/wasmtime-${WASMTIME_VERSION}-${target}"; \
    wasmtime --version

# Login shells reset PATH from /etc/profile; make sure cargo/rustc
# are on PATH in every shell the agent spawns.
RUN printf 'export PATH="/usr/local/cargo/bin:$PATH"\n' \
    > /etc/profile.d/rust.sh && chmod 0644 /etc/profile.d/rust.sh

# Non-root user. The agent runs as `agent` (uid 1000). Shared group
# `gba` (gid 3000) matches the services container so /task files stay
# mutually readable+writable on the shared volume.
RUN groupadd -g 3000 gba && \
    useradd -m -u 1000 -s /bin/bash -G gba agent

# Oracle thin HTTP client. The real reference emulator lives in the
# services sidecar; this is just the client.
COPY --chown=root:root --chmod=0755 quickstart/task/oracle-client /usr/local/bin/oracle

# ─── Task environment ────────────────────────────────────────────────
WORKDIR /task

# Spec (read-only for agent)
COPY --chown=root:root --chmod=0444 spec/ABI.md spec/
COPY --chown=root:root --chmod=0444 spec/gba_bios_stub.bin spec/

# GBATEK — vendored (see spec/vendor-gbatek.sh).
COPY --chown=root:root --chmod=0444 spec/gbatek.htm spec/

# Dev ROMs. Homebrew games NOT in the grader corpus (grader testcases
# reference celeste, chip-advance, goodboy, heartwrench, spout, waimanu
# as held-out games).
COPY --chown=root:root --chmod=0444 corpus/roms/homebrew/anguna.gba        dev-roms/
COPY --chown=root:root --chmod=0444 corpus/roms/homebrew/another-world.gba dev-roms/
COPY --chown=root:root --chmod=0444 corpus/roms/homebrew/xniq.gba          dev-roms/
COPY --chown=root:root --chmod=0444 corpus/roms/homebrew/meteorain.gba     dev-roms/
COPY --chown=root:root --chmod=0444 corpus/roms/homebrew/trogdor.gba       dev-roms/

# Task prompt (read-only)
COPY --chown=root:root --chmod=0444 quickstart/task/TASK.md .

# ─── Git repo init ────────────────────────────────────────────────────
# Init as root, hand /task (minus .git/hooks) to agent. Hooks dir is
# root-owned + empty so the agent can't inject a post-commit hook.
RUN git config --global user.email "agent@gba-eval" && \
    git config --global user.name "agent" && \
    git init && \
    git config core.hooksPath /usr/share/gba-eval/empty-hooks && \
    mkdir -p /usr/share/gba-eval/empty-hooks && \
    chmod 0555 /usr/share/gba-eval/empty-hooks && \
    printf 'target/\n.oracle-out/\n' > /task/.gitignore && \
    git add -A && git commit -m "initial task environment" -q && \
    git config core.sharedRepository 0664 && \
    chown -R agent:gba /task && \
    chown -R root:root /task/spec /task/dev-roms /task/TASK.md && \
    find /task -type d -exec chmod 2775 {} + && \
    find /task -type f \
        -not -path "/task/spec/*" -not -path "/task/dev-roms/*" \
        -not -path "/task/TASK.md" -exec chmod 0664 {} + && \
    chmod -R a-w,a+rX /task/spec /task/dev-roms && \
    chmod a-w,a+r /task/TASK.md

ENV RUST_BACKTRACE=1 \
    HOME=/home/agent \
    CARGO_HOME=/home/agent/.cargo \
    RUSTUP_HOME=/usr/local/rustup

# Pre-create state dirs owned by `agent` so any agent CLI you install
# later can write into $HOME without hitting EACCES.
RUN mkdir -p /home/agent/.claude /home/agent/.codex /home/agent/.config \
             /home/agent/.cache /home/agent/.local/bin /home/agent/.cargo \
    && chown -R agent:agent /home/agent

USER agent

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["sleep", "infinity"]
