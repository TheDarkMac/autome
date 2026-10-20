# syntax=docker/dockerfile:1
#
# Image du backend HTTP `autome-api` (F10.1).
#
# Multi-stage : un builder Rust complet (avec toolchain + gcc requis par
# `ring`, la backend crypto de rustls) compile le binaire en release ;
# l'image finale ne contient que le binaire sur un `debian:bookworm-slim`
# minimal, exécuté par un utilisateur non-root. Les dépendances Cargo sont
# compilées dans une couche à part (cargo-chef, #219) : un changement de
# source ne recompile que les crates du workspace.
#
# Build (contexte = racine du dépôt) :
#   docker build -f docker/api.Dockerfile -t autome-api .

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
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked -p autome-api --recipe-path recipe.json

# Tout le workspace est copié (les crates internes sont des dépendances
# de chemin). `.dockerignore` garde le contexte léger (pas de target/) :
# le `target/` des dépendances compilées ci-dessus est conservé.
COPY . .

# `--locked` : on respecte le Cargo.lock versionné → build reproductible.
# Seuls les crates du workspace sont recompilés.
RUN cargo build --release --locked -p autome-api \
    && strip target/release/autome-api

# ───────────────────────── Runtime ─────────────────────────
# Tag lisible : debian:bookworm-slim (digest épinglé, cf. ci-dessus).
FROM debian:bookworm-slim@sha256:0104b334637a5f19aa9c983a91b54c89887c0984081f2068983107a6f6c21eeb AS runtime

# `ca-certificates` : indispensable pour la validation TLS des appels
# sortants des connecteurs (reqwest + rustls). `--no-install-recommends`
# pour garder l'image fine.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home autome

COPY --from=builder /app/target/release/autome-api /usr/local/bin/autome-api

# Bind sur toutes les interfaces pour être joignable hors du conteneur
# (le défaut Dev `127.0.0.1` ne le serait pas). Le profil et les secrets
# (APP_PROFILE=prod, JWT_SECRET, DATABASE_URL, …) sont fournis à l'exécution.
ENV APP_HTTP_ADDR=0.0.0.0:8080
EXPOSE 8080

USER autome
ENTRYPOINT ["autome-api"]
