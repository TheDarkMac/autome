#!/bin/sh
# Configuration d'exécution du frontend (#282) — lancé par l'entrypoint de
# l'image nginx (`/docker-entrypoint.d/*.sh`) avant nginx.
#
# Écrit /config.js depuis AUTOME_API_URL : l'URL de l'API n'est plus figée au
# build, une même image sert staging et production. Sans la variable, le
# config.js de l'image (objet vide) reste : le bundle retombe sur VITE_API_URL
# du build, sinon http://localhost:8080.
#
# La valeur est inlinée dans du JavaScript : seule une URL
# `http(s)://hôte[:port][/chemin]` sur une ligne est acceptée (ni guillemet,
# ni antislash, ni retour à la ligne). Sinon, refus de démarrer :
# l'entrypoint (`set -e`) arrête le conteneur.
set -eu

target=/usr/share/nginx/html/config.js
url="${AUTOME_API_URL:-}"

if [ -z "$url" ]; then
  echo "$0: AUTOME_API_URL absent, config.js de l'image conservé (VITE_API_URL du build, sinon http://localhost:8080)" >&2
  exit 0
fi

# Sans barre finale : le client ajoute `/auth/…`, `/workflows`…
url="${url%/}"

if [ "$(printf '%s' "$url" | wc -l)" -ne 0 ] ||
  ! printf '%s' "$url" | grep -Eq '^https?://[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[A-Za-z0-9._~/-]*)?$'; then
  echo "$0: AUTOME_API_URL invalide, attendu http(s)://hôte[:port][/chemin]" >&2
  exit 1
fi

printf 'window.__AUTOME_CONFIG__ = { apiUrl: "%s" };\n' "$url" > "$target"
echo "$0: URL de l'API : $url"
