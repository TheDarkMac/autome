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
FROM node:20-bookworm-slim AS builder

WORKDIR /app

# Couche dépendances : ne dépend que du manifeste → cache tant qu'il
# ne change pas. (Pas de package-lock.json versionné → `npm install`.)
COPY package.json ./
RUN npm install

COPY . .

# URL du backend inlinée dans le bundle au build (défaut : même origine).
ARG VITE_API_URL=""
ENV VITE_API_URL=$VITE_API_URL
RUN npm run build

# ───────────────────────── Runtime ─────────────────────────
FROM nginx:1.27-alpine AS runtime

# Config SPA (fallback try_files vers index.html).
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=builder /app/dist /usr/share/nginx/html

EXPOSE 80
# nginx:alpine fournit déjà un CMD/ENTRYPOINT lançant nginx au premier plan.
