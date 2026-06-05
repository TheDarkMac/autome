# syntax=docker/dockerfile:1
#
# Image du frontend Autome (F10.1) — build Vite statique servi par nginx.
#
# Multi-stage : Node compile la SPA (`tsc -b && vite build` → ./dist),
# puis nginx alpine sert les assets statiques. `VITE_API_URL` est injecté
# au **build** (Vite l'inline dans le bundle).
#
# Build (contexte = frontend/) :
#   docker build -f frontend/Dockerfile \
#     --build-arg VITE_API_URL=https://api.example.com -t autome-frontend frontend

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

# URL du backend inlinée dans le bundle au build (défaut : même origine).
ARG VITE_API_URL=""
ENV VITE_API_URL=$VITE_API_URL
RUN pnpm run build

# ───────────────────────── Runtime ─────────────────────────
# Tag lisible : nginx:1.27-alpine (digest épinglé, cf. ci-dessus).
FROM nginx:1.27-alpine@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10 AS runtime

# Config SPA (fallback try_files vers index.html).
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=builder /app/dist /usr/share/nginx/html

EXPOSE 80
# nginx:alpine fournit déjà un CMD/ENTRYPOINT lançant nginx au premier plan.
