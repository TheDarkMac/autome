# syntax=docker/dockerfile:1
#
# Images du backend Rust (F10.1) : une cible par binaire.
#   - `api`    : backend HTTP `autome-api` ;
#   - `worker` : exécuteur de runs `autome-worker`, *stateless* (il consomme
#                la file, F5.1, et délègue au moteur).
#
# Multi-stage : un builder Rust complet (avec toolchain + gcc requis par
# `ring`, la backend crypto de rustls) compile les binaires en release ;
# chaque image finale ne contient que son binaire sur un `debian:bookworm-slim`
# minimal, exécuté par un utilisateur non-root. Les dépendances Cargo sont
# compilées dans une couche à part (cargo-chef, #219) : un changement de
# source ne recompile que les crates du workspace.
#
# Les deux cibles partagent les étages `chef` / `planner` / `builder` (#263) :
# dépendances et crates du workspace compilées une fois pour les deux
# binaires, une seule couche à mettre en cache (scope `gha` `backend`).
# `docker buildx bake api worker` ne construit le builder qu'une fois.
#
# Build (contexte = racine du dépôt, réduit par `.dockerignore`) :
#   docker build -f docker/backend.Dockerfile --target api -t autome-api .
#   docker build -f docker/backend.Dockerfile --target worker -t autome-worker .

# ───────────────────────── Chef ─────────────────────────
# Image de base pinnée par digest (reproductibilité supply-chain, #162).
# Tag lisible : rust:1.90-bookworm. Pour rafraîchir le digest :
#   docker buildx imagetools inspect rust:1.90-bookworm
FROM rust:1.90-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS chef

# cargo-chef (#219) sépare la compilation des dépendances de celle du
# workspace. Version pinnée, installée avec son propre Cargo.lock (`--locked`).
RUN cargo install cargo-chef --locked --version 0.1.78

WORKDIR /app

# ───────────────────────── Planner ─────────────────────────
# Réduit le workspace à sa « recette » (manifests + Cargo.lock) : modifier un
# `.rs` ne change pas `recipe.json`, donc pas la couche des dépendances.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ───────────────────────── Builder ─────────────────────────
FROM chef AS builder

# Dépendances seules, compilées depuis la recette : cette couche reste en
# cache (local ou `gha`) tant que Cargo.toml / Cargo.lock ne changent pas.
# Les deux binaires d'un coup : mêmes features unifiées que `cargo build`
# sur les deux paquets juste en dessous, donc la couche sert aux deux.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked -p autome-api -p autome-worker --recipe-path recipe.json

# Le contexte ne contient que ce que lit `cargo build` (manifests, `crates/`,
# `migrations/` embarquées par `include_str!`) : un commit de doc ou de
# frontend ne touche pas cette couche (#263). Le `target/` des dépendances
# compilées ci-dessus est conservé.
COPY . .

# `--locked` : on respecte le Cargo.lock versionné → build reproductible.
# Seuls les crates du workspace sont recompilés, une fois pour les deux.
RUN cargo build --release --locked -p autome-api -p autome-worker \
    && strip target/release/autome-api target/release/autome-worker

# ───────────────────────── Runtime ─────────────────────────
# Base commune des deux images.
# Tag lisible : debian:bookworm-slim (digest épinglé, cf. ci-dessus).
FROM debian:bookworm-slim@sha256:0104b334637a5f19aa9c983a91b54c89887c0984081f2068983107a6f6c21eeb AS runtime

# `ca-certificates` : indispensable pour la validation TLS des appels
# sortants des connecteurs (reqwest + rustls). `--no-install-recommends`
# pour garder l'image fine.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home autome

# ───────────────────────── Worker ─────────────────────────
FROM runtime AS worker

COPY --from=builder /app/target/release/autome-worker /usr/local/bin/autome-worker

# Configuration par environnement (QUEUE_URL, DATABASE_URL, APP_PROFILE…)
# injectée à l'exécution par compose (F10.2) / k8s (F10.3). Seul port : les
# sondes `/livez` et `/readyz` (#192), joignables hors du conteneur (le
# défaut `127.0.0.1:9091` ne le serait pas).
ENV AUTOME_WORKER_PROBE_ADDR=0.0.0.0:9091
EXPOSE 9091

USER autome
ENTRYPOINT ["autome-worker"]

# ───────────────────────── API ─────────────────────────
# Dernière cible : celle d'un `docker build` sans `--target`.
FROM runtime AS api

COPY --from=builder /app/target/release/autome-api /usr/local/bin/autome-api

# Bind sur toutes les interfaces pour être joignable hors du conteneur
# (le défaut Dev `127.0.0.1` ne le serait pas). Le profil et les secrets
# (APP_PROFILE=prod, JWT_SECRET, DATABASE_URL, …) sont fournis à l'exécution.
ENV APP_HTTP_ADDR=0.0.0.0:8080
EXPOSE 8080

USER autome
ENTRYPOINT ["autome-api"]
