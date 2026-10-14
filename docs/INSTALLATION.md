# 🛠️ Guide d'installation & de démarrage — Autome

> Guide détaillé pour installer, configurer et lancer Autome. Pour une vue
> d'ensemble du projet, voir le [README](../README.md) ; pour l'architecture,
> [`ARCHITECTURE.md`](ARCHITECTURE.md).

Trois voies sont décrites :

- **[Voie A — Docker Compose](#voie-a--docker-compose)** : tout en conteneurs, idéale pour essayer/développer.
- **[Voie B — local](#voie-b--installation-locale)** : exécuter les services directement (toolchains natives).
- **[Voie C — Kubernetes](#voie-c--kubernetes)** : déploiement orchestré.

---

## 1. Prérequis

| Voie | Outils requis |
|---|---|
| A (Compose) | **Docker** + **Docker Compose v2** |
| B (local) | **Rust ≥ 1.85** (edition 2024), **Node ≥ 20** + npm, un **PostgreSQL** accessible, (optionnel) **RabbitMQ**, **Docker** pour `scripts/check-migrations.sh` |
| C (k8s) | Un cluster **Kubernetes**, `kubectl` (≥ 1.27, embarque `kustomize`), un Ingress Controller NGINX |

> ⚠️ **MSRV** : la chaîne de dépendances exige la feature Cargo `edition2024`,
> stabilisée en **Rust 1.85**. Une toolchain 1.80–1.84 échoue à la compilation.
> Vérifier avec `rustc --version` ; mettre à jour via `rustup update`.

---

## 2. Configuration

La configuration se fait **uniquement par variables d'environnement** (lues au
démarrage par `Config::from_env`, cf. `crates/core/src/config.rs`). Copier le
modèle puis adapter :

```bash
cp .env.example .env
```

| Variable | Rôle | Défaut `dev` | En `prod` |
|---|---|---|---|
| `APP_PROFILE` | Profil d'exécution : `dev` ou `prod` | `dev` | `prod` |
| `APP_HTTP_ADDR` | Adresse d'écoute HTTP de l'API | `127.0.0.1:8080` | `0.0.0.0:8080` |
| `DATABASE_URL` | DSN PostgreSQL | `postgres://postgres:postgres@localhost:5432/autome` | **obligatoire** (≠ défaut) |
| `QUEUE_URL` | URL de la file (RabbitMQ) | *(vide)* | recommandé |
| `JWT_SECRET` | Clé de signature des JWT | `dev-insecure-jwt-secret` | **obligatoire** (non-défaut) |
| `ENCRYPTION_KEY` | Clé de chiffrement des credentials au repos | `dev-insecure-encryption-key` | **obligatoire** (non-défaut) |
| `AUTOME_TLS_CERT` | Chemin du certificat PEM (active HTTPS) | *(non défini)* | optionnel |
| `AUTOME_TLS_KEY` | Chemin de la clé privée PEM | *(non défini)* | optionnel |

### Règles du profil `prod` (fail-fast)

En `APP_PROFILE=prod`, le démarrage **échoue immédiatement** si :

- `JWT_SECRET` ou `ENCRYPTION_KEY` est vide **ou** laissé à sa valeur de dev ;
- `DATABASE_URL` est laissé au DSN de dev.

C'est volontaire : aucun secret de développement ne peut fuiter en production.

### TLS (HTTPS)

L'API sert en HTTPS **si et seulement si** `AUTOME_TLS_CERT` **et**
`AUTOME_TLS_KEY` sont tous deux positionnés (décision *all-or-nothing* : en
définir un seul provoque une erreur au démarrage). Sinon, HTTP en clair.

---

## Voie A — Docker Compose

La stack complète (base, file, API, worker, frontend, **Prometheus + Grafana**)
démarre en une commande :

```bash
docker compose up --build
```

| Service | URL | Identifiants |
|---|---|---|
| API | http://localhost:8080 | — (`/health`, `/metrics`) |
| Frontend | http://localhost:8081 | — |
| Grafana | http://localhost:3000 | `admin` / `admin` |
| Prometheus | http://localhost:9090 | — |
| RabbitMQ (UI) | http://localhost:15672 | `guest` / `guest` |
| PostgreSQL | `localhost:5432` | `postgres` / `postgres` |

Vérification rapide :

```bash
curl http://localhost:8080/health     # -> ok
curl http://localhost:8080/metrics    # -> métriques Prometheus
```

Arrêt et nettoyage (supprime aussi les volumes de données) :

```bash
docker compose down -v
```

> Le compose tourne en profil `dev` avec des secrets de développement —
> **ne pas l'exposer en production tel quel**.

---

## Voie B — Installation locale

### B.1 Base de données & migrations

Le schéma est versionné dans `migrations/` (`0001_users.sql` →
`0005_indexes_constraints.sql`). Pour valider/appliquer les migrations sur un
PostgreSQL **jetable** (conteneur Docker dédié, sans effet sur une base
existante) :

```bash
scripts/check-migrations.sh
```

Pour une base que vous gérez vous-même, appliquez les fichiers dans l'ordre
lexicographique, par exemple :

```bash
for f in migrations/*.sql; do psql "$DATABASE_URL" -f "$f"; done
```

### B.2 Backend (API + worker)

```bash
# Compilation + tests de l'ensemble du workspace
cargo build --workspace
cargo test  --workspace

# API HTTP (lit .env / l'environnement)
cargo run -p autome-api          # écoute sur 127.0.0.1:8080 (profil dev)

# Worker — exécuteur de runs (dans un autre terminal)
cargo run -p autome-worker
```

### B.3 Frontend

```bash
cd frontend
npm install
npm run dev        # serveur de dev Vite (port affiché dans la console)
```

Pour un build de production statique : `npm run build` (sortie dans `dist/`).
L'URL du backend est injectée au build via `VITE_API_URL`.

### B.4 HTTPS local (optionnel)

```bash
export AUTOME_TLS_CERT=/chemin/cert.pem
export AUTOME_TLS_KEY=/chemin/key.pem
cargo run -p autome-api          # -> https://…
```

---

## Voie C — Kubernetes

Les manifests sont assemblés par **kustomize** dans `k8s/` (namespace `autome`).

### C.1 Rendu & secrets

```bash
# Rendu local (validation, sans appliquer)
kubectl kustomize k8s/
```

⚠️ `k8s/config.yaml` contient des secrets de **démonstration** (`REPLACE_ME…`).
Avant tout usage réel, remplacez-les (idéalement via SealedSecrets / External
Secrets / Vault) : `JWT_SECRET`, `ENCRYPTION_KEY`, `POSTGRES_PASSWORD`,
`DATABASE_URL`, `QUEUE_URL`.

### C.2 Images

Les déploiements référencent les images publiées sur GHCR par la CD
(F10.5) : `ghcr.io/thedarkmac/autome-{api,worker,frontend}`. La CD épingle le
tag immuable `sha-<court>` ; pour un déploiement manuel, ajustez les `image:`
des manifests (ou via `kubectl set image`).

### C.3 Déploiement

```bash
kubectl apply -k k8s/
kubectl -n autome rollout status deployment/api
kubectl -n autome get pods,svc
```

### C.4 Accès

L'Ingress NGINX route :

| Hôte | Service |
|---|---|
| `autome.local` | frontend |
| `api.autome.local` | api |

Ajoutez ces hôtes à votre `/etc/hosts` (pointant vers l'IP de l'ingress) pour
un essai local.

Vérification :

```bash
kubectl -n autome port-forward svc/api 8080:8080 &
curl http://localhost:8080/health     # -> ok
```

### C.5 Monitoring

Le namespace inclut **Prometheus** (scrape de l'API) et **Grafana** (datasource
+ dashboard *Autome — Overview* provisionnés) :

```bash
kubectl -n autome port-forward svc/grafana 3000:3000     # http://localhost:3000 (admin/admin)
kubectl -n autome port-forward svc/prometheus 9090:9090  # http://localhost:9090
```

---

## 3. Tests & qualité (DoD)

La CI rejoue exactement la « definition of done » appliquée à chaque feature :

```bash
# Backend
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Frontend
cd frontend
npm run typecheck
npm run lint
npm run test       # Vitest
```

---

## 4. Dépannage

| Symptôme | Cause probable | Solution |
|---|---|---|
| `feature edition2024 is required` à la compilation | Toolchain Rust < 1.85 | `rustup update` (viser ≥ 1.85) |
| L'API refuse de démarrer en `prod` (`… doit être défini (non-défaut)`) | Secret laissé à la valeur de dev ou `DATABASE_URL` par défaut | Définir de vrais `JWT_SECRET` / `ENCRYPTION_KEY` / `DATABASE_URL` |
| `TLS partiellement configuré` au démarrage | Une seule des deux variables TLS positionnée | Définir `AUTOME_TLS_CERT` **et** `AUTOME_TLS_KEY`, ou aucune |
| Port déjà utilisé (`address already in use`) | 8080/8081/3000/9090/5432/15672 occupé | Libérer le port ou ajuster `APP_HTTP_ADDR` / les mappings du compose |
| `docker compose` échoue à joindre `db`/`queue` | Services pas encore *healthy* | Attendre les healthchecks (`depends_on … service_healthy`) puis relancer |
| Pods `api`/`worker` en `ImagePullBackOff` | Tag/registry d'image absent | Vérifier que les images GHCR existent (CD passée) ou ajuster les `image:` |

---

## Voir aussi

- [README](../README.md) — vue d'ensemble & quick start.
- [ARCHITECTURE.md](ARCHITECTURE.md) — architecture détaillée + diagrammes.
- [adr/](adr/) — décisions d'architecture.
- [ROADMAP.md](../claude-code/ROADMAP.md) — suivi des features.
- [.env.example](../.env.example) — modèle de configuration.
