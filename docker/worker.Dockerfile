# syntax=docker/dockerfile:1
#
# Image de l'exécuteur de runs `autome-worker` (F10.1).
#
# Même schéma multi-stage que `api.Dockerfile`. Le worker est *stateless* :
# il consomme la file (F5.1) et délègue au moteur. Seul port : ses sondes
# `/livez` et `/readyz` pour l'orchestrateur (#192).
#
# Build (contexte = racine du dépôt) :
#   docker build -f docker/worker.Dockerfile -t autome-worker .

# ───────────────────────── Builder ─────────────────────────
# Image de base pinnée par digest (reproductibilité supply-chain, #162).
# Tag lisible : rust:1.90-bookworm. Pour rafraîchir le digest :
#   docker buildx imagetools inspect rust:1.90-bookworm
FROM rust:1.90-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS builder

WORKDIR /app
COPY . .

RUN cargo build --release --locked -p autome-worker \
    && strip target/release/autome-worker

# ───────────────────────── Runtime ─────────────────────────
# Tag lisible : debian:bookworm-slim (digest épinglé, cf. ci-dessus).
FROM debian:bookworm-slim@sha256:0104b334637a5f19aa9c983a91b54c89887c0984081f2068983107a6f6c21eeb AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home autome

COPY --from=builder /app/target/release/autome-worker /usr/local/bin/autome-worker

# Configuration par environnement (QUEUE_URL, DATABASE_URL, APP_PROFILE…)
# injectée à l'exécution par compose (F10.2) / k8s (F10.3). Sondes joignables
# hors du conteneur (le défaut `127.0.0.1:9091` ne le serait pas).
ENV AUTOME_WORKER_PROBE_ADDR=0.0.0.0:9091
EXPOSE 9091

USER autome
ENTRYPOINT ["autome-worker"]
