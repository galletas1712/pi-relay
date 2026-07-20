FROM rust:1-bookworm AS builder
WORKDIR /src
COPY rust rust
RUN cargo build --manifest-path rust/Cargo.toml --release -p agent-daemon -p agent-runtime

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends btrfs-progs ca-certificates git rsync \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/rust/target/release/pi-agentd /usr/local/bin/pi-agentd
COPY --from=builder /src/rust/target/release/pi-runtime /usr/local/bin/pi-runtime
COPY PI.md PI.compaction.md /opt/pi-relay/
COPY subagent-roles /opt/pi-relay/subagent-roles
COPY workflows /opt/pi-relay/workflows
WORKDIR /opt/pi-relay

