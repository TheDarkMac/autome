# syntax=docker/dockerfile:1
#
# Image du frontend Autome (F10.1) — build Vite statique servi par nginx.
#
# Multi-stage : Node compile la SPA (`tsc -b && vite build` → ./dist),
# puis nginx alpine sert les assets statiques. L'URL de l'API se donne **à
# l'exécution** (#282) : `AUTOME_API_URL`, écrite dans `/config.js` au
# démarrage du conteneur — une même image sert staging et production.
# `VITE_API_URL` (build) ne sert plus que de repli.
#
# Build (contexte = frontend/) et lancement :
#   docker build -f frontend/Dockerfile -t autome-frontend frontend
#   docker run -p 8081:80 -e AUTOME_API_URL=https://api.example.com autome-frontend

# ───────────────────────── Builder ─────────────────────────
# Image de base pinnée par digest (reproductibilité supply-chain, #162).
# Tag lisible : node:20-bookworm-slim. Pour mettre à jour le digest :
#   docker buildx imagetools inspect node:20-bookworm-slim
FROM node:20-bookworm-slim@sha256:2cf067cfed83d5ea958367df9f966191a942351a2df77d6f0193e162b5febfc0 AS builder

WORKDIR /app

# pnpm via Corepack (version pinnée par `packageManager` dans package.json).
ENV CI=true
RUN corepack enable

# Couche dépendances : manifeste + lockfile → cache tant qu'ils ne changent
# pas. `--frozen-lockfile` (équivalent `npm ci`) = installs déterministes.
COPY package.json pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile

COPY . .

# Repli inliné au build, si AUTOME_API_URL est absente à l'exécution (vide :
# http://localhost:8080, cf. `apiBaseUrl()`).
ARG VITE_API_URL=""
ENV VITE_API_URL=$VITE_API_URL
RUN pnpm run build

# ───────────────────────── Runtime ─────────────────────────
# Tag lisible : nginx:1.27-alpine (digest épinglé, cf. ci-dessus).
FROM nginx:1.27-alpine@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10 AS runtime

# Config SPA (fallback try_files vers index.html).
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=builder /app/dist /usr/share/nginx/html
# Écrit /config.js depuis AUTOME_API_URL au démarrage (#282), lancé par
# l'entrypoint de l'image avant nginx.
COPY --chmod=755 docker/40-autome-config.sh /docker-entrypoint.d/

EXPOSE 80
# nginx:alpine fournit déjà un CMD/ENTRYPOINT lançant nginx au premier plan.
