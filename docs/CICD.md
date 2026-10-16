# ✅ Checklist CI/CD & reproductibilité

> Synthèse de la chaîne d'intégration/livraison continue d'Autome et des
> garanties de **reproductibilité** des builds. Audit de l'existant
> (`.github/workflows/`, `docker/`, `k8s/`, `Cargo.lock`) + écarts connus.

---

## 1. Pipelines

### CI — `.github/workflows/ci.yml` (F10.4)

Déclenchée sur **push** et **pull request** vers `main`. Reproduit la
*definition of done* appliquée à chaque feature.

| Job | Étapes |
|---|---|
| **Backend (Rust)** | services `postgres:16-alpine` + `rabbitmq:3-management-alpine` (healthchecks) · toolchain pinnée + cache · `cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `scripts/check-migrations.sh` (migrations + seed + invariants) · `cargo test --workspace --locked -- --nocapture` avec `TEST_DATABASE_URL` / `TEST_AMQP_URL` · garde-fou « aucun test réel sauté » · `cargo build --workspace --release --locked` |
| **Frontend (Node)** | Corepack/pnpm · `pnpm install --frozen-lockfile` · `pnpm run typecheck` · `pnpm run lint` · `pnpm run test` (Vitest) |
| **Smoke e2e (Docker Compose)** | `docker/bake-action` construit les images `api` / `worker` depuis le compose (surcharge `docker/compose.ci.yml`, cache `gha` partagé avec la CD) · `docker compose up --no-build --wait api worker` (PostgreSQL + RabbitMQ, healthchecks) · `scripts/compose-smoke.sh` · journaux des conteneurs si échec · `down --volumes` toujours — #216 |

- `concurrency` avec `cancel-in-progress` (annule les runs obsolètes).
- `permissions: contents: read` (moindre privilège).
- Tests backend surtout in-process / in-memory, frontend sous jsdom.
- Tests **réels** (#218) : `crates/persistence/tests/pg_*.rs` et `rabbitmq.rs`,
  `crates/worker/tests/pg_owner_credentials.rs`, test PostgreSQL de
  `crates/api/src/runtime.rs`. Ils se sautent sans `TEST_DATABASE_URL` /
  `TEST_AMQP_URL` (en local) ; le job `backend` les fournit au niveau du job,
  pointées sur ses services. Variable fournie mais service injoignable ⇒ ces
  tests **échouent** (connexion en `expect`), ils ne se sautent pas.
- Garde-fou : l'étape *Real-service tests were not skipped* échoue si la
  sortie des tests contient un message de saut PostgreSQL/RabbitMQ, et liste
  les binaires de tests réels exécutés (preuve dans les logs).
- `scripts/check-migrations.sh` démarre son propre PostgreSQL jetable
  (`postgres:16-alpine`, surchargeable par `PGIMAGE`) : schéma vierge, toutes
  les migrations dans l'ordre + seed de dev, puis invariants (tables, contraintes).
- Job `smoke` : images réelles sur la stack Compose. Inscription, connexion, workflow,
  run déclenché via l'API puis exécuté par le worker jusqu'à `succeeded` —
  couvre le câblage (env, migrations au démarrage, Dockerfiles, healthchecks,
  sélecteurs de backend). Borné à 45 min (compilation Rust complète des deux images
  dès qu'une source change).

### CD — `.github/workflows/cd.yml` (F10.5)

Déclenchée **après une CI verte** (`workflow_run` sur `main`), sur les **tags**
`v*.*.*` (release), ou manuellement (`workflow_dispatch`).

| Job | Rôle |
|---|---|
| **build-push** | matrice `api` / `worker` / `frontend` → `docker/build-push-action`, publication sur **GHCR** (`ghcr.io/<owner>/autome-*`) via `GITHUB_TOKEN` (`packages: write`), tags via `metadata-action`, cache `gha` |
| **deploy** | `environment: production`, épingle l'image (`sha-<court>`) dans `k8s/`, `kubectl kustomize` (validation systématique), puis `kubectl apply -k` **uniquement si** le secret `KUBE_CONFIG` est présent (sinon dry-run documenté) |

Garde-fous : `build-push` ne s'exécute pas si la CI déclenchante a échoué ;
`deploy` dépend de `build-push` ; déploiement réel derrière un *environment*
protégé + secret optionnel.

---

## 2. Reproductibilité — versions

| Aspect | État | Référence |
|---|---|---|
| Lockfile Rust | `Cargo.lock` **versionné** | racine du dépôt |
| Builds Rust verrouillés | `--locked` en CI (`clippy`, `test`, `build`) **et** dans les Dockerfiles | `ci.yml`, `docker/*.Dockerfile` |
| Lockfile frontend | `pnpm-lock.yaml` **versionné** + `pnpm install --frozen-lockfile` (CI + Docker) | `frontend/`, `ci.yml`, `frontend/Dockerfile` |
| Toolchain Rust | **pinnée 1.90** (CI `dtolnay/rust-toolchain@1.90` + images `rust:1.90-bookworm`) | `ci.yml`, `docker/*` |
| MSRV | `rust-version = "1.85"` (plancher edition2024) | `Cargo.toml` |
| Node / pnpm | Node **20** (CI `setup-node@v4` + image `node:20-…`) ; pnpm via Corepack (`packageManager` pinné) | `ci.yml`, `frontend/` |
| Images de base | pinnées par **digest `@sha256:`** (tag lisible en commentaire) | `docker/*.Dockerfile`, `frontend/Dockerfile` |

> La cohérence toolchain CI ↔ images (1.90) garantit que « vert en CI » ⇒
> « build identique en conteneur ».

---

## 3. Reproductibilité — conteneurs & déploiement

- **Images multi-stage** : builder lourd jeté, runtime minimal non-root
  (cf. [INSTALLATION](INSTALLATION.md) / F10.1).
- **Bases pinnées par digest `@sha256:`** (#162) : `rust:1.90-bookworm`,
  `debian:bookworm-slim`, `node:20-bookworm-slim`, `nginx:1.27-alpine` — le tag
  lisible reste en commentaire. Rafraîchir un digest :
  `docker buildx imagetools inspect <image:tag>`.
- **Tags d'images immuables** : la CD pousse `sha-<court>` (commit) en plus de
  `latest` / semver, et **déploie le tag `sha`** → un déploiement pointe vers un
  artefact précis et traçable (pas le `latest` mouvant).
- **Manifests** assemblés par `kustomize` (`kubectl kustomize k8s/`) — rendu
  déterministe, validé en CD avant tout `apply`.

---

## 4. Configuration & secrets

- Configuration **par variables d'environnement** uniquement (`Config::from_env`).
- **Aucun secret en dépôt** : `.gitignore` exclut `.env`, `*.pem`, `*.key`
  (seul `.env.example` est versionné).
- En `prod`, démarrage **fail-fast** si un secret reste à sa valeur de dev
  (cf. [INSTALLATION §2](INSTALLATION.md#2-configuration)).
- k8s : secrets en `Secret` avec valeurs `REPLACE_ME` à remplacer
  (SealedSecrets / External Secrets / Vault).

---

## 5. Checklist actionnable

**Intégration (CI)**
- [x] `cargo fmt --all --check` vert.
- [x] `cargo clippy --workspace --all-targets -- -D warnings` vert.
- [x] `cargo test --workspace --locked` vert.
- [x] `cargo build --workspace --release --locked` vert.
- [x] Frontend : `typecheck` + `lint` + `test` verts.
- [x] CI déclenchée sur push **et** PR vers `main`.
- [x] Smoke de bout en bout sur la stack Compose (images réelles, PostgreSQL, RabbitMQ) — #216.
- [x] Tests PostgreSQL/RabbitMQ réels (`TEST_DATABASE_URL` / `TEST_AMQP_URL`) exécutés en CI, avec garde-fou contre le saut silencieux — #218.
- [x] Migrations + seed validés sur un PostgreSQL vierge en CI (`scripts/check-migrations.sh`) — #218.

**Livraison (CD)**
- [x] Images `api`/`worker`/`frontend` construites et poussées sur GHCR.
- [x] Tag d'image **immuable** (`sha-<court>`) déployé (pas `latest`).
- [x] CD conditionnée à une **CI verte**.
- [x] Déploiement réel **gardé** (environment + secret kubeconfig).

**Reproductibilité**
- [x] `Cargo.lock` versionné + `--locked` partout (CI + Docker).
- [x] Toolchain Rust et Node **pinnées** et cohérentes CI ↔ images.
- [x] Aucun secret committé.
- [x] Frontend : lockfile `pnpm-lock.yaml` versionné + `pnpm install --frozen-lockfile` (CI + Dockerfile) — #162.
- [x] Images de base pinnées par **digest** `@sha256:` (CI/Docker) — #162.

---

## 6. Écarts connus & recommandations

| # | Écart | État |
|---|---|---|
| 1 | **Frontend non verrouillé** : CI et `frontend/Dockerfile` utilisaient `npm install` (résolution non déterministe) | ✅ **Résolu (#162)** : le repo a un `pnpm-lock.yaml` versionné ; CI + Dockerfile passent à **`pnpm install --frozen-lockfile`** (équivalent `npm ci`). Standardisation sur **pnpm** plutôt qu'un `package-lock.json` concurrent (cohérent avec le lockfile déjà présent). |
| 2 | **Bases d'images par tag** (`debian:bookworm-slim`, `node:20-…`, etc.) : un tag peut bouger | ✅ **Résolu (#162)** : pinnées par **digest** `@sha256:` dans les 3 Dockerfiles (tag lisible conservé en commentaire). Mise à jour : `docker buildx imagetools inspect <image:tag>`. |
| 3 | `cargo fmt`/`clippy` sans `--locked` | ✅ **Résolu (#162)** : `--locked` ajouté à l'étape `clippy` (la CI échoue désormais si `Cargo.lock` n'est pas en phase dès le lint). |
| 4 | **Branchement runtime** (PostgreSQL/RabbitMQ) non câblé dans les binaires | ✅ Résolu — [dette #107](https://github.com/TheDarkMac/autome/issues/107) close (API + worker branchés sur PG/RabbitMQ via `AUTOME_PERSISTENCE`/`AUTOME_QUEUE`). |
| 5 | **Aucun smoke de bout en bout en CI** : `scripts/compose-smoke.sh` n'était lancé qu'à la main | ✅ **Résolu (#216)** : job `smoke` (images construites, `compose up --wait`, smoke, journaux si échec). Reste : la compilation Rust est refaite à chaque changement de source (`COPY . .` invalide le cache) — un étage de dépendances (cargo-chef) l'éviterait, CI et CD comprises. |
| 6 | **Tests PostgreSQL/RabbitMQ réels jamais exécutés en CI** : gatés par `TEST_DATABASE_URL` / `TEST_AMQP_URL`, ils passaient « au vert » sans rien exécuter ; `scripts/check-migrations.sh` (deux fois réparé après coup, #186 et #208) n'était lancé qu'à la main | ✅ **Résolu (#218)** : services du job `backend`, variables au niveau du job, garde-fou sur la sortie des tests, script de migrations en CI. Reste : images des services par tag (comme `docker-compose.yml` et `k8s/`), test réseau `TEST_HTTP_URL` (connectors) toujours sauté en CI. |

---

## Voir aussi

- [README](../README.md) · [INSTALLATION.md](INSTALLATION.md) · [ARCHITECTURE.md](ARCHITECTURE.md) · [ROADMAP.md](../claude-code/ROADMAP.md)
- Workflows : [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) · [`.github/workflows/cd.yml`](../.github/workflows/cd.yml)
