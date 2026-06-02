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
| **Backend (Rust)** | toolchain pinnée + cache · `cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo test --workspace --locked` · `cargo build --workspace --release --locked` |
| **Frontend (Node)** | `npm install` · `npm run typecheck` · `npm run lint` · `npm run test` (Vitest) |

- `concurrency` avec `cancel-in-progress` (annule les runs obsolètes).
- `permissions: contents: read` (moindre privilège).
- Aucun service externe : tests in-process / in-memory + jsdom.

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
| Builds Rust verrouillés | `--locked` en CI (`test`, `build`) **et** dans les Dockerfiles | `ci.yml`, `docker/*.Dockerfile` |
| Toolchain Rust | **pinnée 1.90** (CI `dtolnay/rust-toolchain@1.90` + images `rust:1.90-bookworm`) | `ci.yml`, `docker/*` |
| MSRV | `rust-version = "1.85"` (plancher edition2024) | `Cargo.toml` |
| Node | **20** (CI `setup-node@v4` + image `node:20-bookworm-slim`) | `ci.yml`, `frontend/Dockerfile` |

> La cohérence toolchain CI ↔ images (1.90) garantit que « vert en CI » ⇒
> « build identique en conteneur ».

---

## 3. Reproductibilité — conteneurs & déploiement

- **Images multi-stage** : builder lourd jeté, runtime minimal non-root
  (cf. [INSTALLATION](INSTALLATION.md) / F10.1).
- **Bases pinnées par tag** : `rust:1.90-bookworm`, `debian:bookworm-slim`,
  `node:20-bookworm-slim`, `nginx:1.27-alpine`.
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

**Livraison (CD)**
- [x] Images `api`/`worker`/`frontend` construites et poussées sur GHCR.
- [x] Tag d'image **immuable** (`sha-<court>`) déployé (pas `latest`).
- [x] CD conditionnée à une **CI verte**.
- [x] Déploiement réel **gardé** (environment + secret kubeconfig).

**Reproductibilité**
- [x] `Cargo.lock` versionné + `--locked` partout (CI + Docker).
- [x] Toolchain Rust et Node **pinnées** et cohérentes CI ↔ images.
- [x] Bases d'images pinnées par tag.
- [x] Aucun secret committé.
- [ ] Frontend : `package-lock.json` versionné + `npm ci` *(cf. §6)*.
- [ ] Images de base pinnées par **digest** `@sha256:` *(cf. §6)*.

---

## 6. Écarts connus & recommandations

| # | Écart | Recommandation |
|---|---|---|
| 1 | **Frontend non verrouillé** : pas de `package-lock.json` versionné ; CI et `frontend/Dockerfile` utilisent `npm install` (résolution non déterministe) | Committer un `package-lock.json` et passer à **`npm ci`** (CI + Dockerfile) pour des builds frontend reproductibles. |
| 2 | **Bases d'images par tag** (`debian:bookworm-slim`, `node:20-…`, etc.) : un tag peut bouger | Pinner par **digest** (`image@sha256:…`) pour une reproductibilité bit-à-bit, avec une routine de mise à jour. |
| 3 | `cargo fmt`/`clippy` sans `--locked` | Cosmétique ; ajouter `--locked` pour l'uniformité si souhaité. |
| 4 | **Branchement runtime** (PostgreSQL/RabbitMQ) non câblé dans les binaires | Suivi par la [dette #107](https://github.com/TheDarkMac/autome/issues/107) ; sans impact sur la reproductibilité des builds. |

> Ces écarts sont **documentés** ici plutôt que corrigés : F11.5 est une feature
> de documentation. Les points 1 et 2 méritent chacun une feature/issue dédiée.

---

## Voir aussi

- [README](../README.md) · [INSTALLATION.md](INSTALLATION.md) · [ARCHITECTURE.md](ARCHITECTURE.md) · [ROADMAP.md](../ROADMAP.md)
- Workflows : [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) · [`.github/workflows/cd.yml`](../.github/workflows/cd.yml)
