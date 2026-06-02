# syntax=docker/dockerfile:1
#
# Image de l'exécuteur de runs `autome-worker` (F10.1).
#
# Même schéma multi-stage que `api.Dockerfile`. Le worker est *stateless* :
# il consomme la file (F5.1) et délègue au moteur ; il ne sert pas de port
# HTTP, d'où l'absence d'`EXPOSE`.
#
# Build (contexte = racine du dépôt) :
#   docker build -f docker/worker.Dockerfile -t autome-worker .

# ───────────────────────── Builder ─────────────────────────
FROM rust:1.90-bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release --locked -p autome-worker \
    && strip target/release/autome-worker

# ───────────────────────── Runtime ─────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home autome

COPY --from=builder /app/target/release/autome-worker /usr/local/bin/autome-worker

# Configuration par environnement (QUEUE_URL, DATABASE_URL, APP_PROFILE…)
# injectée à l'exécution par compose (F10.2) / k8s (F10.3).
USER autome
ENTRYPOINT ["autome-worker"]
