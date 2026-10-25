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
| **Backend (Rust)** | services `postgres:16-alpine` + `rabbitmq:3-management-alpine` pinnés par digest (healthchecks, #263) · toolchain pinnée + cache · `cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `scripts/check-migrations.sh` (migrations + seed + invariants) · `cargo llvm-cov --no-report --workspace --locked -- --nocapture` (tests instrumentés) avec `TEST_DATABASE_URL` / `TEST_AMQP_URL` / `TEST_HTTP_URL` · garde-fou « aucun test réel sauté » (PostgreSQL, RabbitMQ, réseau) · rapport de couverture (résumé du job + artefact `coverage-backend`) · plancher `--fail-under-lines` / `--fail-under-functions` · `cargo build --workspace --release --locked` |
| **Frontend (Node)** | Corepack/pnpm · `pnpm install --frozen-lockfile` · `pnpm run typecheck` · `pnpm run lint` · `pnpm run test:coverage` (Vitest, couverture v8 et planchers de `vite.config.ts`) · rapport de couverture (résumé du job + artefact `coverage-frontend`) |
| **Smoke e2e (Docker Compose)** | `docker/bake-action` construit les images `api` / `worker` depuis le compose (surcharge `docker/compose.ci.yml`), deux cibles de `docker/backend.Dockerfile` dont l'étage builder est construit une fois (cache `gha`, scope `backend` partagé avec la CD, #263) · `docker compose up --no-build --wait api worker` (PostgreSQL + RabbitMQ + Jaeger, healthchecks) · `scripts/compose-smoke.sh` (`SMOKE_REQUIRE_TRACES=1` : trace du run exigée dans Jaeger, #249) · journaux des conteneurs si échec · `down --volumes` toujours — #216 |

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
- **Couverture chiffrée** (#247), sans service tiers :
  - backend : `cargo llvm-cov` remplace `cargo test` (tests exécutés une seule
    fois, instrumentés, toujours contre les services réels du job). Total puis
    une ligne par crate (lignes, fonctions, régions) dans le résumé du job,
    rendu par `scripts/coverage-summary.sh rust` ; `lcov.info` +
    `coverage.json` en artefact (14 jours).
  - frontend : `vitest run --coverage` (fournisseur v8) sur tout `src/`
    (fichiers non testés compris ; exclus : tests, `apiSchema.ts` généré,
    `main.tsx`), lignes / instructions / fonctions / branches dans le résumé,
    `lcov.info` + rapport HTML en artefact.
  - **Planchers bloquants** juste sous la mesure courante : options
    `--fail-under-*` de l'étape *Coverage threshold* (`ci.yml`) et
    `test.coverage.thresholds` (`frontend/vite.config.ts`). On les remonte
    quand la couverture monte ; on ne les baisse pas pour faire passer une PR.
  - En local : `cargo llvm-cov --workspace` (`cargo install cargo-llvm-cov` +
    `rustup component add llvm-tools-preview`), `pnpm run test:coverage`.
- `scripts/check-migrations.sh` démarre son propre PostgreSQL jetable
  (`postgres:16-alpine`, surchargeable par `PGIMAGE`) : schéma vierge, toutes
  les migrations dans l'ordre + seed de dev, puis invariants (tables, contraintes).
- Job `smoke` : images réelles sur la stack Compose. Inscription, connexion, workflow,
  run déclenché via l'API puis exécuté par le worker jusqu'à `succeeded`, puis
  retrouvé dans Jaeger en une seule trace (requête API, mise en file, run et
  nœud côté worker — #249) — couvre le câblage (env, migrations au démarrage, Dockerfiles, healthchecks,
  sélecteurs de backend). Borné à 45 min : compilation Rust complète des deux
  images à froid (≈ 25 min), seulement les crates du workspace quand
  `Cargo.toml` / `Cargo.lock` n'ont pas changé (≈ 6 min, cargo-chef — #219).

### CD — `.github/workflows/cd.yml` (F10.5)

Déclenchée **après une CI verte** (`workflow_run` sur `main`), sur les **tags**
`v*.*.*` (release), ou manuellement (`workflow_dispatch`).

| Job | Rôle |
|---|---|
| **release** | fixe **une fois** le commit à livrer (#278) : `workflow_run.head_sha` — le commit validé par la CI, pas le dernier de `main` (`github.sha`) —, sinon `github.sha` (tag, relance manuelle) ; sorties `sha`, `tag` (`sha-<7>`) et `owner` (minuscules pour GHCR), commit affiché dans le résumé |
| **build-push** | checkout du commit de `release`, matrice `api` / `worker` / `frontend` → `docker/build-push-action`, publication sur **GHCR** (`ghcr.io/<owner>/autome-*`) via `GITHUB_TOKEN` (`packages: write`), tags via `metadata-action` (`sha-<7>` et label `org.opencontainers.image.revision` repris de `release`), cache `gha` (scope `backend` de la CI pour `api` / `worker`, écrit par `api` seulement ; `frontend`) |
| **deploy** | `environment: production`, checkout du même commit, épingle l'image (tag de `release`, pas recalculé) dans `k8s/`, `kubectl kustomize` de l'overlay du cluster (variable de dépôt `K8S_OVERLAY`, ex. `clusters/prod` ; vide = `k8s/`) — validation systématique —, puis **uniquement si** le secret `KUBE_CONFIG` est présent (sinon dry-run documenté) : tout sauf le worker, attente de l'API (qui migre la base), puis le worker (#271) |

Garde-fous : sur `workflow_run`, `release` — donc toute la CD — ne s'exécute
que pour une CI **verte** lancée par un **push** sur ce dépôt : le filtre
`branches: [main]` porte sur le nom de la branche de tête, si bien qu'une PR
ouverte depuis la branche `main` d'un fork déclenche aussi la CD, qui
construirait sinon son code ; `deploy` dépend de `release` et `build-push` ;
déploiement réel derrière un *environment* protégé + secret optionnel.

---

## 2. Reproductibilité — versions

| Aspect | État | Référence |
|---|---|---|
| Lockfile Rust | `Cargo.lock` **versionné** | racine du dépôt |
| Builds Rust verrouillés | `--locked` en CI (`clippy`, `llvm-cov`, `build`) **et** dans les Dockerfiles | `ci.yml`, `docker/backend.Dockerfile` |
| Lockfile frontend | `pnpm-lock.yaml` **versionné** + `pnpm install --frozen-lockfile` (CI + Docker) | `frontend/`, `ci.yml`, `frontend/Dockerfile` |
| Toolchain Rust | **pinnée 1.90** (CI `dtolnay/rust-toolchain@1.90` + images `rust:1.90-bookworm`) | `ci.yml`, `docker/*` |
| MSRV | `rust-version = "1.85"` (plancher edition2024) | `Cargo.toml` |
| Node / pnpm | Node **20** (CI `setup-node@v4` + image `node:20-…`) ; pnpm via Corepack (`packageManager` pinné) | `ci.yml`, `frontend/` |
| Images de base | pinnées par **digest `@sha256:`** (tag lisible en commentaire) | `docker/backend.Dockerfile`, `frontend/Dockerfile` |
| Images tierces de la pile | `postgres`, `rabbitmq` pinnées par **digest** (#263) | `ci.yml` (services), `docker-compose.yml`, `k8s/`, `scripts/check-migrations.sh` |

> La cohérence toolchain CI ↔ images (1.90) garantit que « vert en CI » ⇒
> « build identique en conteneur ».

---

## 3. Reproductibilité — conteneurs & déploiement

- **Images multi-stage** : builder lourd jeté, runtime minimal non-root
  (cf. [INSTALLATION](INSTALLATION.md) / F10.1).
- **Dépendances Cargo dans une couche à part** (#219) : étages `chef`
  (`cargo install cargo-chef --locked --version 0.1.78`), `planner`
  (`cargo chef prepare` → `recipe.json`, dérivée des manifests et de
  `Cargo.lock` seulement) et `builder` (`cargo chef cook --release --locked`,
  puis `COPY . .` et `cargo build --release --locked`). Modifier un `.rs` ne
  change pas la recette : la couche des dépendances compilées est reprise du
  cache (local ou `gha`, `mode=max`) et seuls les crates du workspace sont
  recompilés. Rafraîchir cargo-chef : changer la version dans
  `docker/backend.Dockerfile`.
- **Un Dockerfile, deux cibles** (#263) : `docker/backend.Dockerfile` cuit et
  compile `autome-api` et `autome-worker` dans un même étage `builder` ; les
  cibles `api` et `worker` n'en copient que leur binaire. Dépendances et
  crates du workspace sont compilés une fois pour les deux, `bake` ne
  construit le builder qu'une fois, et un seul scope de cache (`backend`)
  sert la CI et la CD.
- **Contexte de build en liste blanche** (`.dockerignore`, #263) :
  `Cargo.toml`, `Cargo.lock`, `crates/`, `migrations/`. Un commit de doc, de
  frontend ou de manifests ne change pas le contexte, donc ne recompile pas
  les binaires (avant : 254 s de rebuild pour un commit de ROADMAP).
- **Profil release sans LTO entre crates** (#263, `Cargo.toml`) : l'édition
  des liens en `lto = "thin"` dominait chaque rebuild. Même profil pour la CI
  et la prod (le smoke teste le binaire livré). Mesure locale, 4 cœurs,
  `cargo build --release -p autome-api -p autome-worker` :

  | Profil | À froid | `core` modifié | `worker` modifié | api / worker |
  |---|---|---|---|---|
  | `lto = "thin"`, `codegen-units = 1` (avant) | 1591 s | 485 s | 167 s | 14,1 / 10,0 Mo |
  | `lto = "thin"`, `codegen-units = 16` | 1426 s | 690 s | 240 s | 17,7 / 12,0 Mo |
  | **sans LTO, `codegen-units = 1`** (retenu) | **784 s** | **207 s** | **20 s** | **13,5 / 9,7 Mo** |
  | sans LTO, `codegen-units = 16` | 680 s | 198 s | 25 s | 17,1 / 11,8 Mo |

  Pas de benchmark d'exécution dans le dépôt : le coût est jugé faible pour
  un service surtout en attente réseau / base, dont le code chaud (argon2,
  ring, serde) est optimisé dans son propre crate. À remesurer si un
  chemin CPU apparaît.
- **Bases pinnées par digest `@sha256:`** (#162) : `rust:1.90-bookworm`,
  `debian:bookworm-slim`, `node:20-bookworm-slim`, `nginx:1.27-alpine` — le tag
  lisible reste en commentaire. Rafraîchir un digest :
  `docker buildx imagetools inspect <image:tag>`.
- **Tags d'images immuables** : la CD pousse `sha-<court>` (commit) en plus de
  `latest` / semver, et **déploie le tag `sha`** → un déploiement pointe vers un
  artefact précis et traçable (pas le `latest` mouvant).
- **Manifests** assemblés par `kustomize` (`kubectl kustomize k8s/`) — rendu
  déterministe, validé en CD avant tout `apply`. Ce qui dépend du cluster
  (proxys de confiance, NetworkPolicy de l'API) vit dans un overlay
  `clusters/<cluster>/` (modèle : `clusters/example/`, #271).
- **Ordre de déploiement** : l'API migre la base, le worker non ; la CD
  applique le worker une fois l'API prête, et le worker attend de toute façon
  que le schéma porte toutes ses migrations (`AUTOME_WORKER_SCHEMA_WAIT_SECS`,
  #271).

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
- [x] `cargo test --workspace --locked` vert (via `cargo llvm-cov`).
- [x] `cargo build --workspace --release --locked` vert.
- [x] Frontend : `typecheck` + `lint` + `test` verts.
- [x] Couverture chiffrée backend et frontend dans le résumé du job, avec plancher bloquant — #247.
- [x] CI déclenchée sur push **et** PR vers `main`.
- [x] Smoke de bout en bout sur la stack Compose (images réelles, PostgreSQL, RabbitMQ) — #216.
- [x] Tests PostgreSQL/RabbitMQ réels (`TEST_DATABASE_URL` / `TEST_AMQP_URL`) exécutés en CI, avec garde-fou contre le saut silencieux — #218.
- [x] Test réseau de `ReqwestHttpClient` (`TEST_HTTP_URL`, cible HTTPS publique) exécuté en CI, sous le même garde-fou — #263.
- [x] Migrations + seed validés sur un PostgreSQL vierge en CI (`scripts/check-migrations.sh`) — #218.

**Livraison (CD)**
- [x] Images `api`/`worker`/`frontend` construites et poussées sur GHCR.
- [x] Tag d'image **immuable** (`sha-<court>`) déployé (pas `latest`).
- [x] CD conditionnée à une **CI verte**.
- [x] CD sur le **commit validé par la CI** (`workflow_run.head_sha`), tag d'image calculé une fois pour build et déploiement ; CI de PR (fork) exclue — #278.
- [x] Déploiement réel **gardé** (environment + secret kubeconfig).
- [x] Migrations appliquées (API) **avant** le déploiement du worker ; le worker attend un schéma à jour — #271.
- [ ] Overlay `clusters/<cluster>/` créé (proxys de confiance au CIDR de l'ingress, NetworkPolicy adaptée) et désigné par `K8S_OVERLAY` — à faire par cluster (#271).

**Reproductibilité**
- [x] `Cargo.lock` versionné + `--locked` partout (CI + Docker).
- [x] Toolchain Rust et Node **pinnées** et cohérentes CI ↔ images.
- [x] Aucun secret committé.
- [x] Frontend : lockfile `pnpm-lock.yaml` versionné + `pnpm install --frozen-lockfile` (CI + Dockerfile) — #162.
- [x] Images de base pinnées par **digest** `@sha256:` (CI/Docker) — #162.
- [x] Dépendances Cargo compilées dans une couche Docker réutilisée tant que `Cargo.toml` / `Cargo.lock` ne changent pas (cargo-chef pinné) — #219.
- [x] Images des services de la CI, de compose et de `k8s/` pinnées par digest — #263.
- [x] Une compilation Rust pour les deux images, un seul scope de cache, contexte de build réduit à ce que lit `cargo` — #263.

---

## 6. Écarts connus & recommandations

| # | Écart | État |
|---|---|---|
| 1 | **Frontend non verrouillé** : CI et `frontend/Dockerfile` utilisaient `npm install` (résolution non déterministe) | ✅ **Résolu (#162)** : le repo a un `pnpm-lock.yaml` versionné ; CI + Dockerfile passent à **`pnpm install --frozen-lockfile`** (équivalent `npm ci`). Standardisation sur **pnpm** plutôt qu'un `package-lock.json` concurrent (cohérent avec le lockfile déjà présent). |
| 2 | **Bases d'images par tag** (`debian:bookworm-slim`, `node:20-…`, etc.) : un tag peut bouger | ✅ **Résolu (#162)** : pinnées par **digest** `@sha256:` dans les 3 Dockerfiles (tag lisible conservé en commentaire). Mise à jour : `docker buildx imagetools inspect <image:tag>`. |
| 3 | `cargo fmt`/`clippy` sans `--locked` | ✅ **Résolu (#162)** : `--locked` ajouté à l'étape `clippy` (la CI échoue désormais si `Cargo.lock` n'est pas en phase dès le lint). |
| 4 | **Branchement runtime** (PostgreSQL/RabbitMQ) non câblé dans les binaires | ✅ Résolu — [dette #107](https://github.com/TheDarkMac/autome/issues/107) close (API + worker branchés sur PG/RabbitMQ via `AUTOME_PERSISTENCE`/`AUTOME_QUEUE`). |
| 5 | **Aucun smoke de bout en bout en CI** : `scripts/compose-smoke.sh` n'était lancé qu'à la main | ✅ **Résolu (#216)** : job `smoke` (images construites, `compose up --wait`, smoke, journaux si échec). La compilation des dépendances, refaite à chaque changement de source (`COPY . .` invalidait le cache), est isolée dans un étage cargo-chef réutilisé par le cache `gha` de la CI et de la CD (#219) : rebuild après modification d'un `.rs` mesuré à 386 s au lieu de 1918 s en local (4 cœurs). |
| 6 | **Tests PostgreSQL/RabbitMQ réels jamais exécutés en CI** : gatés par `TEST_DATABASE_URL` / `TEST_AMQP_URL`, ils passaient « au vert » sans rien exécuter ; `scripts/check-migrations.sh` (deux fois réparé après coup, #186 et #208) n'était lancé qu'à la main | ✅ **Résolu (#218)** : services du job `backend`, variables au niveau du job, garde-fou sur la sortie des tests, script de migrations en CI. Les restes (images des services par tag, test réseau `TEST_HTTP_URL` sauté) sont soldés par #263. |
| 7 | **Rebuild des images coûteux** (#219 l'avait ramené à 386 s) : un scope de cache `gha` par image, donc dépendances et crates du workspace compilés deux fois ; contexte de build ouvert (`.dockerignore` n'excluait `*.md` qu'à la racine) ; édition des liens en LTO thin | ✅ **Résolu (#263)** : `docker/backend.Dockerfile` à deux cibles et un scope `backend`, `.dockerignore` en liste blanche, profil release sans LTO entre crates (§3). |

---

## Voir aussi

- [README](../README.md) · [INSTALLATION.md](INSTALLATION.md) · [ARCHITECTURE.md](ARCHITECTURE.md) · [ROADMAP.md](../claude-code/ROADMAP.md)
- Workflows : [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) · [`.github/workflows/cd.yml`](../.github/workflows/cd.yml)
