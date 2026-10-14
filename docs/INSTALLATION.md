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
| `QUEUE_URL` | URL de la file (RabbitMQ) | *(vide)* | **obligatoire** (avec `AUTOME_QUEUE=rabbitmq`) |
| `AUTOME_PERSISTENCE` | Stores de l'API et du worker : `memory` ou `postgres` | `memory` | **`postgres` obligatoire** |
| `AUTOME_QUEUE` | File des runs : `memory` ou `rabbitmq` | `memory` | **`rabbitmq` obligatoire** |
| `AUTOME_MIGRATE` | L'API applique les migrations au démarrage (`true`/`1`/`yes`, mode `postgres`) | *(non)* | recommandé |
| `JWT_SECRET` | Clé de signature des JWT | `dev-insecure-jwt-secret` | **obligatoire** (non-défaut) |
| `ENCRYPTION_KEY` | Clé de chiffrement des credentials au repos — **identique pour l'API et le worker** | `dev-insecure-encryption-key` | **obligatoire** (non-défaut) |
| `AUTOME_TLS_CERT` | Chemin du certificat PEM (active HTTPS) | *(non défini)* | optionnel |
| `AUTOME_TLS_KEY` | Chemin de la clé privée PEM | *(non défini)* | optionnel |
| `AUTOME_EGRESS_ALLOWLIST` | Hôtes internes autorisés en sortie HTTP (anti-SSRF), séparés par virgule | *(vide : public uniquement)* | selon besoin |
| `AUTOME_CONNECTOR_CREDENTIALS` | Worker : `per_user` (token du propriétaire du run) ou `single_tenant` (repli sur `AUTOME_CONNECTOR_TOKEN_<PROVIDER>`) | `per_user` | `per_user` (instance partagée) |

### Sortie réseau (anti-SSRF)

Les requêtes HTTP sortantes — action `http_request` des workflows (URL choisie
par l'utilisateur) et échanges OAuth — ne peuvent viser que des adresses
**publiques**. Sont refusés : `localhost`/loopback, réseaux privés (RFC 1918,
IPv6 ULA), link-local (dont `169.254.169.254`, métadonnées cloud), CGNAT,
plages réservées/documentation, et tout schéma autre que `http`/`https`.
Le contrôle porte sur l'IP littérale, sur **chaque adresse DNS résolue** (la
connexion part vers l'adresse vérifiée : pas de DNS rebinding) et sur chaque
redirection (5 au plus). Délai 30 s (connexion 10 s), réponse ≤ 10 Mio.

Pour joindre un service interne légitime (IdP on-prem, API intranet), ajoutez
son nom ou son IP à `AUTOME_EGRESS_ALLOWLIST`. Avec un **proxy sortant**
(`HTTPS_PROXY`), ajoutez le proxy à l'allowlist : le filtrage des noms d'hôte
revient alors au proxy, qui fait la résolution DNS.

### Tokens des connecteurs (par utilisateur)

Les actions `slack.*`, `gmail.*`, `notion.*` et `github.*` s'exécutent avec le
token OAuth **du propriétaire du run** (celui du workflow, fixé au
déclenchement). Chaque utilisateur connecte ses comptes via
`POST /credentials/{provider}/authorize` puis `…/callback` ; l'API stocke le
jeton **chiffré** (AES-256-GCM, clé dérivée de `ENCRYPTION_KEY`) et le worker le
relit au moment du run. Le `provider_id` OAuth doit être le préfixe de l'action
(`slack`, `gmail`, `notion`, `github`).

- **Partage API ↔ worker** : `AUTOME_PERSISTENCE=postgres` sur les deux
  services (table `connector_tokens`, migration `0011`) et le **même**
  `ENCRYPTION_KEY`. En `memory`, chaque process a son propre store (dev seul).
- **Pas de token** pour ce propriétaire, ou run sans propriétaire (créé avant
  l'isolation par propriétaire) : le node échoue avec un message explicite ; le
  run ne part jamais avec le compte d'un autre utilisateur.
- **Token expiré** : si l'access token expire dans moins d'une minute (Google :
  environ 1 h après la connexion), le worker le **rafraîchit** avec le refresh
  token stocké, puis re-stocke le nouveau jeton chiffré (le refresh token est
  conservé quand le provider n'en renvoie pas). Il faut donc fournir au worker
  la **même config OAuth que l'API** (`AUTOME_OAUTH_PROVIDERS` +
  `AUTOME_OAUTH_<ID>_*` ; en k8s, déjà partagée via `envFrom`), et autoriser
  son `TOKEN_URL` en sortie si l'IdP est interne (`AUTOME_EGRESS_ALLOWLIST`).
  Sans refresh token, sans config OAuth sur le worker, ou si le provider refuse
  (`invalid_grant`) : le node échoue en demandant de reconnecter le compte. Un
  seul rafraîchissement à la fois par (utilisateur, provider) **par réplique**
  de worker.
- **Mode mono-tenant (opt-in)** : `AUTOME_CONNECTOR_CREDENTIALS=single_tenant`
  sur le worker rétablit le repli sur un token **global** par provider
  (`AUTOME_CONNECTOR_TOKEN_SLACK`, `…_GMAIL`, `…_NOTION`, `…_GITHUB`) quand le
  propriétaire n'a pas connecté le sien. Ce token est alors utilisé par **tous**
  les utilisateurs : réservé à une instance à un seul tenant. Une erreur de
  lecture du store, ou un token expiré impossible à rafraîchir, ne déclenche
  jamais ce repli. Le worker l'annonce au
  démarrage ; toute autre valeur que `per_user`/`single_tenant` est refusée.

### Règles du profil `prod` (fail-fast)

En `APP_PROFILE=prod`, le démarrage **échoue immédiatement** si :

- `JWT_SECRET` ou `ENCRYPTION_KEY` est vide **ou** laissé à sa valeur de dev ;
- `DATABASE_URL` est laissé au DSN de dev ;
- `AUTOME_PERSISTENCE` ou `AUTOME_QUEUE` vaut `memory` (défaut) — **API et
  worker**. En mémoire, chaque process a sa propre file et ses propres stores :
  les runs publiés par l'API ne seraient jamais vus du worker, et comptes,
  workflows et tokens disparaîtraient au redémarrage. Le message nomme la
  variable à positionner (`AUTOME_PERSISTENCE=postgres`, `AUTOME_QUEUE=rabbitmq`).

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

## Authentification (comptes & jetons)

L'API émet ses propres JWT (HS256, signés avec `JWT_SECRET`). Les endpoints de
comptes sont publics ; tout le reste exige `Authorization: Bearer <access_token>`.

| Méthode | Path | Body | Réponse |
|---|---|---|---|
| POST | `/auth/signup` | `{"email","password"}` | 201 + paire de jetons · 400 saisie invalide · 409 email déjà pris |
| POST | `/auth/login` | `{"email","password"}` | 200 + paire de jetons · 401 identifiants invalides · 429 trop d'échecs (`Retry-After`) |
| POST | `/auth/refresh` | `{"refresh_token"}` | 200 + nouvelle paire · 401 refresh invalide, expiré ou déjà utilisé |

- Paire de jetons : `{"access_token","refresh_token","token_type":"Bearer","expires_in":900}`.
  Access valable 15 min, refresh 7 jours et **à usage unique** (rotation).
- Mot de passe : 8 à 1024 caractères, haché en **Argon2id**. L'email est
  normalisé (trim + minuscules).
- Un compte créé par `signup` reçoit le rôle **editor** (CRUD workflows, runs,
  credentials) — pas d'administration.
- Anti-bruteforce : 5 tentatives sans succès par email et par fenêtre de 15 min,
  puis 429. Compteur **par réplique** (en mémoire).
- **Isolation par propriétaire** : un workflow appartient à son créateur, un run
  au propriétaire de son workflow. Les listes ne montrent que ses propres
  ressources ; lire, modifier, lancer ou supprimer la ressource d'autrui répond
  comme si elle n'existait pas (404, ou 204 pour `DELETE`). Seul un super-admin
  (permission `*`) voit tout. Les lignes antérieures à la migration `0010`
  (sans propriétaire) ne sont visibles que des super-admins.

```bash
curl -s -H 'content-type: application/json' \
  -d '{"email":"me@example.com","password":"correct-horse"}' \
  http://localhost:8080/auth/signup
```

Base de dev seedée (`migrations/seeds/dev_seed.sql`) : compte super-admin
`dev@autome.local` / `autome-dev-password` (à ne jamais appliquer en prod).

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
