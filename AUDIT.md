# Audit logiciel complet — Autome

> Auditeur : revue senior (architecture, sécurité, qualité, DevOps, tests).
> Méthode : lecture du code source réel du dépôt. Chaque constat cite les
> fichiers/lignes observés. Aucune conclusion n'est extrapolée hors du code.
> Date : 2026-06-06 — branche `main` (commit `cdb1b05`).

Légende des constats : ✅ point fort · ⚠️ amélioration recommandée · ❌ problème détecté.
Gravité sécurité : **Critique / Élevée / Moyenne / Faible**. Priorité dette : **P0 / P1 / P2 / P3**.

---

# 1. Executive Summary

**Autome** est une plateforme d'automatisation de workflows (type *n8n / Make /
Zapier*) : un backend **Rust** (workspace Cargo multi-crates) exécute des graphes
de *nodes* (triggers/actions/logique) déclenchés par événement, via une file de
messages et un pool de workers stateless ; un frontend **React/TypeScript** offre
un éditeur visuel. Le tout est outillé (PostgreSQL, RabbitMQ, Prometheus/Grafana,
Docker, Kubernetes, GitHub Actions).

**Niveau de maturité : avancé sur l'ingénierie, incomplet sur le produit.** Le
code est d'une qualité remarquable — découplage par traits, injection de
dépendances systématique, `unsafe` interdit, clippy en `-D warnings`, **529
fonctions de test** backend + **23 fichiers de test** frontend, crypto et JWT
soignés. Mais plusieurs **chaînes fonctionnelles ne sont pas câblées de bout en
bout** : il n'existe **aucun endpoint d'authentification** (login/signup/refresh)
alors que le hachage de mot de passe et le frontend les attendent ; les workflows
**n'ont pas d'isolation par propriétaire** ; l'action HTTP générique est **exposée
au SSRF**. Le projet est mûr pour du *staging* de démonstration, **pas pour la
production** en l'état.

### Points forts principaux
- ✅ Architecture hexagonale exemplaire : `core` agnostique (traits + erreurs),
  adaptateurs concrets isolés (`persistence`, `connectors`, `api`).
- ✅ Sécurité « par construction » sur les briques implémentées : AES-256-GCM
  avec envelope versionnée + rotation (`crates/core/src/secrets/cipher.rs`),
  JWT HS256 avec révocation par `jti` + rotation refresh anti-rejeu
  (`crates/core/src/auth/jwt.rs`), Argon2id (`crates/core/src/auth/password.rs`).
- ✅ Discipline de tests et de CI très élevée, horloge injectable pour le
  déterminisme, secrets redactés dans `Debug`.

### Points faibles principaux
- ❌ **Cycle de vie d'authentification absent** côté HTTP (P0).
- ❌ **Broken Object Level Authorization** : pas de tenant/owner sur workflows & runs (Élevée).
- ❌ **SSRF** sur l'action `http_request` et le client reqwest sans filtrage d'URL (Élevée).
- ❌ Stores critiques **in-memory par défaut** (révocation, permissions, idempotence,
  tokens OAuth) — l'OAuth reste in-memory **même en mode postgres** (Moyenne/Élevée).
- ⚠️ Observabilité limitée à `eprintln!` (pas de logs structurés/tracing).

### Niveau de risque global : **MOYEN-ÉLEVÉ** pour une mise en production immédiate
(briques de sécurité solides mais surface fonctionnelle incomplète + 2 failles
d'autorisation/SSRF réelles). **FAIBLE** comme base de code à faire évoluer.

### Notes /10

| Axe | Note |
|---|---|
| Architecture | 9 |
| Maintenabilité | 9 |
| Sécurité | 5 |
| Performance | 7 |
| Tests | 8 |
| Scalabilité | 6 |
| Qualité du code | 9 |
| Documentation | 8 |

---

# 2. Compréhension de l'Application

**Ce que fait l'application** (déduit du `README.md`, des crates et de
`docs/ARCHITECTURE.md`) : exécuter des **workflows** modélisés comme des graphes
orientés acycliques (DAG). Un *trigger* (webhook, scheduler cron/intervalle,
polling) crée un *run* placé dans une file ; un pool de workers stateless le
consomme et le déroule via le moteur DAG, en appelant des *connecteurs*
(Slack, Gmail, Notion, GitHub, HTTP générique).

- **Métier adressé** : automatisation / intégration *no-code* entre SaaS — RPA léger.
- **Utilisateurs cibles** : équipes techniques/ops construisant des automatisations ;
  un éditeur visuel (`frontend/src/NodeCanvas.tsx`, `NodePanel.tsx`) vise aussi des
  profils moins techniques.
- **Fonctionnalités principales** (observées dans le code, pas seulement le README) :
  - Moteur DAG : séquence, conditions `If`/`Switch`, boucles `Loop`/`While`,
    parallélisme borné, mapping/templating, politiques d'erreur (retry/backoff/
    timeout/fallback) — `crates/engine/src/exec.rs` (1434 lignes), `condition.rs`,
    `mapping.rs`, `graph.rs`.
  - Connecteurs Slack/Gmail/Notion/GitHub + HTTP + SDK — `crates/connectors/src/`.
  - Orchestration : `core/src/queue.rs` (queue + bornée), `core/src/scheduler.rs`,
    `worker/src/lib.rs` (pool, idempotence), `worker/src/executor.rs`,
    `worker/src/dispatch.rs`.
  - API REST : CRUD workflows (`api/src/workflows.rs`), runs (`api/src/runs.rs`),
    credentials OAuth (`api/src/credentials.rs`), OpenAPI (`api/src/openapi.rs`),
    métriques (`api/src/metrics.rs`).
  - Sécurité : JWT, RBAC, audit, chiffrement, TLS (`api/src/tls.rs`).
- **Fonctionnalités secondaires** : OpenAPI générée, métriques Prometheus +
  dashboards Grafana provisionnés, polling de l'état des runs côté SPA
  (`frontend/src/useRunPolling.test.tsx`, `runs.ts`).

**Flux utilisateur complet (cible)** :
```
Utilisateur ─(éditeur)→ POST /workflows ─→ WorkflowRepository (PG/mem)
   │
Trigger (webhook/scheduler) ─→ RunDispatcher ─→ Queue ─→ WorkerPool
                                                     │
                                          RunExecutor → moteur DAG → connecteurs
                                                     │
                              RunStateStore (checkpoints) ←┘
   │
Utilisateur ─(GET /runs/:id, polling)→ visualisation de l'exécution
```
**Réserve** : ce flux est **partiellement câblé** — il manque l'étape d'obtention
du jeton (pas de `/auth/login`), et l'isolation par utilisateur (cf. §8).

---

# 3. Architecture Technique

### Structure des dossiers (observée)
```
crates/
  core/         types, traits, erreurs (agnostique) : workflow, run_state, queue,
                scheduler, rbac, audit, auth/{jwt,password,revocation}, secrets/{cipher,keys}, config
  engine/       moteur DAG pur (exec, graph, condition, mapping) — dépend de core
  connectors/   Slack/Gmail/Notion/GitHub/HTTP + SDK + OAuth + reqwest_client
  persistence/  adaptateurs PostgreSQL (run_state, workflow_repo, revocation,
                permission, audit) + rabbitmq + pool + migrations runner
  api/          serveur axum (auth, workflows, runs, credentials, metrics, tls, openapi)
  worker/       pool d'exécuteurs (lib, executor, dispatch)
frontend/       SPA React/TS/Vite/Tailwind
migrations/     SQL versionné 0001→0009 (+ seeds)
docker/ k8s/ monitoring/ docs/ scripts/ .github/workflows/
```

### Découpage & responsabilités — ✅ exemplaire
Le graphe de dépendances est **acyclique et orienté vers le domaine** (ports &
adapters / hexagonal) :

```
                 ┌──────────────────────────────┐
                 │            core               │  (traits: Queue, RunStateStore,
                 │  (aucune dép. framework/IO)   │   WorkflowRepository, PermissionStore,
                 └──────────────────────────────┘   RevocationStore, AuditSink, Action…)
                    ▲        ▲        ▲       ▲
        ┌───────────┘        │        │       └───────────┐
        │                    │        │                   │
     engine            connectors  persistence          worker
   (DAG pur)         (Slack/HTTP/  (impl PG des        (pool, exécuteur)
                       OAuth…)      traits core)
        ▲                    ▲        ▲                   ▲
        └────────────────────┴────┬───┴───────────────────┘
                                  │
                                api  (axum ; câble tout via Arc<dyn Trait>)
```

- ✅ **Cœur agnostique vérifié** : `crates/engine/src/exec.rs:96` définit
  `trait NodeRunner` ; le moteur ne connaît ni Slack ni Postgres. L'ADR 0001
  (`docs/adr/0001-choix-framework-backend.md`) confine axum au crate `api` —
  respecté (`api/src/main.rs`).
- ✅ **Inversion de dépendance systématique** : `api/src/runtime.rs:48` assemble
  des `Arc<dyn WorkflowRepository>`, `Arc<dyn RunStateStore>`, `Arc<dyn Queue>`,
  `Arc<dyn RunDispatcher>` ; le choix mémoire/Postgres est une simple variable
  d'env (`PersistenceMode`).
- ✅ **Couplage faible / cohésion forte** : chaque module a une responsabilité
  nette (le `cipher.rs` ne fait que le chiffrement, `jwt.rs` que les tokens).

⚠️ Couplage résiduel : `api/src/runtime.rs` dépend simultanément de `core`,
`persistence` et `connectors` — normal pour la *composition root*, mais c'est le
point le plus chargé du système ; à surveiller.

---

# 4. Technologies Utilisées

| Techno | Rôle | Avantages | Inconvénients | Pertinence |
|---|---|---|---|---|
| **Rust 1.85+ / edition 2021** (`Cargo.toml`) | Langage backend | Sûreté mémoire, perf, `unsafe` interdit | Courbe d'apprentissage, temps de build | ✅ Excellent pour un orchestrateur fiable |
| **axum 0.x + Tokio** | Serveur HTTP async | Écosystème mûr, extractors typés | — | ✅ Standard de facto, justifié en ADR 0001 |
| **tokio-postgres + deadpool** | Accès PG async | Requêtes paramétrées, pool | Pas d'ORM → SQL manuel | ✅ Cohérent avec l'approche « SQL versionné » |
| **lapin (AMQP) + adaptateurs tokio** | RabbitMQ | Découplage, backpressure | Complexité opérationnelle | ✅ Adapté au modèle file→workers (ADR 0003) |
| **jsonwebtoken (HS256)** | JWT | Simple, éprouvé | Symétrique (single-tenant) | ✅ OK ; RS256 à prévoir (cf. doc jwt.rs) |
| **argon2 / password-hash** | Hachage mot de passe | Argon2id, état de l'art | — | ✅ Bon choix (mais non câblé, cf. §8) |
| **aes-gcm + rand (OsRng)** | Chiffrement secrets | AEAD authentifié, nonce frais | Gestion de clés à mûrir | ✅ Bon (ADR 0004) |
| **rustls (ring)** | TLS serveur & client | Pas d'OpenSSL système | — | ✅ Cohérent partout |
| **reqwest blocking (rustls)** | Transport connecteurs | Réutilisable | **Aucun filtrage SSRF** | ⚠️ cf. §8 |
| **utoipa** | OpenAPI à la compilation | Spec toujours à jour | Annotations verboses | ✅ |
| **React + TS + Vite + Tailwind** | SPA éditeur | DX, typage strict | — | ✅ |
| **PostgreSQL / Prometheus / Grafana** | DB / observabilité | Standards | — | ✅ |
| **Docker multi-stage + Kustomize + GitHub Actions** | Build/déploiement | Reproductible, images pinnées par digest | — | ✅ Très soigné |

✅ Résolution centralisée des versions via `[workspace.dependencies]` (`Cargo.toml`)
— garantit une version unique partagée par tous les crates.

---

# 5. Analyse de Qualité du Code

- ✅ **Lisibilité** : documentation de module (`//!`) systématique, explicite sur
  le « pourquoi » et les frontières (`config.rs:1-10`, `cipher.rs:1-29`,
  `jwt.rs:1-23`). Niveau professionnel.
- ✅ **Maintenabilité / modularité** : petites unités cohésives, séparation
  pure/effets de bord (le moteur est testable sans IO).
- ✅ **Gestion d'erreurs** : type `Error` central, `Result` partout, conversions
  contextualisées (`storage_err` dans `persistence/src/run_state.rs:16`).
- ✅ **KISS / YAGNI** : pas de sur-ingénierie ; les abstractions servent un besoin
  réel (seam de test). Le wildcard RBAC est volontairement minimal (`rbac.rs:133`).
- ⚠️ **DRY** — micro-duplications assumées : `percent_encode` dupliqué
  (`oauth/client.rs:151-160`, commentaire le reconnaît) ; le `fresh_jti` réécrit
  un formatage hex (`jwt.rs:60-68`) au lieu d'utiliser `hex` (déjà en dépendance).
  Impact faible. Priorité **P3**.
- ⚠️ **Dette explicitement tracée** : nombreuses mentions « dette #107 »
  (`runtime.rs`, `worker/src/lib.rs:21`) et « viendra avec F5.4 ». C'est sain
  (dette documentée), mais elle est **structurelle** : beaucoup de stores
  in-memory non encore persistés (révocation, idempotence, dedup lock).
- ❌ **Observabilité par `eprintln!`** : les échecs d'audit et d'auth loggent sur
  stderr (`api/src/auth.rs:178`, `workflows.rs:129`) sans `tracing`/structuration
  ni niveaux. Impact : exploitation difficile en prod. Priorité **P1**.

---

# 6. SOLID

| Principe | Score | Constat |
|---|---|---|
| **SRP** | 9/10 | Modules mono-responsabilité (cipher, jwt, rbac, scheduler). |
| **OCP** | 9/10 | Extension par traits sans modifier le cœur : `NodeRunner` (`exec.rs:96`), `HttpClient` (`http.rs:44`), `KeyProvider`, SDK connecteurs. Ajouter un connecteur ≠ toucher le moteur. |
| **LSP** | 8/10 | Impl mémoire/PG des traits interchangeables (`InMemoryRevocationStore` vs `PgRevocationStore`) ; testées contre le même contrat. |
| **ISP** | 9/10 | Traits étroits et ciblés : `CredentialService` expose 5 méthodes utiles (`connectors/src/credentials.rs:75`), `PermissionStore` une seule (`rbac.rs:145`). |
| **DIP** | 9/10 | Toute la couche `api` dépend d'abstractions (`Arc<dyn Trait>`), câblage en composition root (`runtime.rs`). |

- ✅ Exemple OCP/DIP : `EnvelopeCipher<K: KeyProvider>` (`cipher.rs:45`) est
  générique sur le provider de clés → rotation/KMS futurs sans réécriture.
- ⚠️ Légère entorse DIP : `build_credential_service` (`runtime.rs:37`) **fige**
  `InMemoryConnectorTokenStore` même en mode Postgres (cf. §8) — l'abstraction
  existe mais n'est pas exploitée pour brancher le store persistant.

---

# 7. Design Patterns

**Réellement présents :**
- ✅ **Strategy** : `NodeRunner`, `HttpClient`, `DedupLock`, `Clock` injectables.
- ✅ **Repository** : `WorkflowRepository`, `RunStateStore` (`core/src/workflow/repo.rs`, `run_state.rs`).
- ✅ **Adapter / Ports & Adapters** : `persistence` adapte les traits `core` à PG ;
  `reqwest_client.rs` adapte reqwest au trait `HttpClient`.
- ✅ **Builder** : `Role::with_permissions` (`rbac.rs:100`), `AuditEvent::with_resource/with_metadata`.
- ✅ **Dependency Injection** : composition root `runtime.rs`.
- ✅ **Factory** : `AuthState::from_config` (`auth.rs:112`), `OAuthRegistry::from_env`.
- ✅ **Envelope encryption (pattern crypto)** : `cipher.rs` (format `v1:key_id:nonce:ct`).
- ✅ **Blanket impl comme Null Object** : `NoopAuditSink` pour désactiver l'audit.

**Anti-patterns / manques :**
- ❌ **Manque : couche d'autorisation par ressource (policy/guard)** — l'autz est
  uniquement « permission globale », sans contrôle objet (cf. §8). Un pattern
  *policy/ABAC* manque.
- ⚠️ **Manque : Outbox/Saga** pour la cohérence dispatch↔checkpoint (publication
  file + persistance run sont deux effets non transactionnels — `QueueRunDispatcher`).
- ⚠️ Léger **Service Locator implicite** via variables d'env (`AUTOME_PERSISTENCE`,
  `AUTOME_QUEUE`, `AUTOME_MIGRATE`) dispersées — acceptable mais à centraliser.

---

# 8. Analyse de Sécurité

### ❌ [Élevée] Broken Object Level Authorization (BOLA / multi-tenant absent)
- **Fichiers** : `crates/api/src/workflows.rs:146-389`, `migrations/0009_align_ids_text.sql`.
- **Raison technique** : les handlers vérifient une **permission globale**
  (`require_permission(&user, PERM_WORKFLOWS_READ/WRITE)`) mais **jamais la
  propriété de la ressource**. La migration `0009` a **supprimé `workflows.owner_id`**
  (« le graphe n'a pas d'owner ») et les `runs` n'ont **pas de FK ni d'owner**.
- **Impact / exploitation** : tout utilisateur disposant de `workflows.read`
  peut lire/lister **les workflows de tous les autres** ; avec `workflows.write`,
  modifier/supprimer n'importe quel workflow par son id (`PUT/DELETE /workflows/{id}`).
  Idem `GET /runs/{run_id}` (`api/src/runs.rs`) : pas de vérification d'appartenance.
- **Correction** : réintroduire un `owner_id` (ou `tenant_id`) sur `workflows`/`runs`,
  le renseigner depuis `claims.sub`, et filtrer toutes les requêtes par propriétaire
  (ou check explicite + 404). Couvrir par tests d'autorisation négatifs.

### ❌ [Élevée] SSRF via l'action HTTP générique
- **Fichiers** : `crates/connectors/src/http.rs:60-107`, `crates/connectors/src/reqwest_client.rs:45-74`.
- **Raison technique** : `HttpAction::build_request` prend `url` **directement
  depuis les `params` du node** (contrôlés par l'utilisateur) et
  `ReqwestHttpClient::send` l'envoie **sans aucune validation** (pas de
  blocage de `127.0.0.1`, `::1`, `169.254.169.254`, plages RFC1918, schémas
  `file:`/`gopher:`…).
- **Impact / exploitation** : un workflow avec un node `http_request` ciblant
  `http://169.254.169.254/latest/meta-data/` (métadonnées cloud) ou des services
  internes permet l'exfiltration de credentials d'instance / le scan interne /
  l'accès à des admin internes. Le worker exécute ces requêtes côté serveur.
- **Correction** : valider l'URL avant envoi (schéma `http(s)` uniquement,
  résolution DNS + rejet des IP loopback/link-local/privées/ULA, anti-rebinding
  en résolvant puis en se connectant à l'IP validée), allowlist optionnelle par
  tenant, timeouts et taille de réponse bornés. À appliquer aussi aux connecteurs.

### ❌ [Élevée/Maturité] Cycle de vie d'authentification non câblé
- **Fichiers** : routes réelles = `api/src/main.rs:49-68` + `auth.rs:305-310`
  (uniquement `/auth/whoami`, `/auth/admin`) ; `core/src/auth/password.rs`
  (Argon2id, **jamais appelé hors tests**) ; contrat frontend
  `frontend/src/auth.ts:11-14` attend `/auth/login|signup|refresh`.
- **Raison technique** : aucun endpoint n'émet de JWT (`issue_access` n'apparaît
  qu'en tests). Aucun handler ne lit `users.password_hash`. `PgPermissionStore`
  existe mais **rien ne peuple les permissions** via HTTP.
- **Impact** : l'API **ne peut pas authentifier un vrai utilisateur** de bout en
  bout ; le frontend appelle des routes inexistantes. Bloquant produit.
- **Correction** : implémenter `/auth/signup` (Argon2id → `users`), `/auth/login`
  (vérif mot de passe → `issue_access`/`issue_refresh`), `/auth/refresh`
  (`TokenService::rotate`), et une route d'admin d'attribution de rôles. **P0**.

### ❌ [Moyenne] Tokens OAuth jamais persistés (même en mode Postgres)
- **Fichiers** : `crates/api/src/runtime.rs:37-44`.
- **Raison** : `build_credential_service` instancie **toujours**
  `InMemoryConnectorTokenStore`, quel que soit `PersistenceMode`.
- **Impact** : credentials connecteurs perdus à chaque redémarrage et non
  partagés entre répliques → connecteurs inutilisables en prod multi-instances.
- **Correction** : brancher un `ConnectorTokenStore` PG (chiffré au repos) quand
  `mode == Postgres`.

### ⚠️ [Moyenne] Validation du `state` OAuth déléguée au client
- **Fichiers** : `connectors/src/credentials.rs:87-94` (`complete_authorize` ne
  prend pas `state`), `oauth/client.rs:80-90`.
- **Raison** : le `state` (anti-CSRF) est généré serveur mais sa **revalidation**
  au callback repose sur le frontend (`sessionStorage`, `frontend/src/connectors.ts:217`).
- **Impact** : si un intégrateur omet la vérification, CSRF sur la liaison de compte.
- **Correction** : stocker `state`+`code_verifier` côté serveur (par session) et
  les valider dans `complete_authorize`.

### ⚠️ [Moyenne] Refresh token en `localStorage`
- **Fichiers** : `frontend/src/session.ts:4-5` (compromis assumé en commentaire).
- **Impact** : exfiltration possible via XSS. **Correction** : cookie `HttpOnly`
  + `SameSite`, ou rotation courte + révocation (déjà supportée côté backend).

### ⚠️ [Faible/Moyenne] Dérivation de clé de chiffrement par SHA-256 simple
- **Fichiers** : `api/src/runtime.rs:38` (`Sha256::digest(encryption_key)`).
- **Raison** : SHA-256 d'une passphrase n'est pas un KDF ; acceptable **si** la
  clé est un secret aléatoire 256 bits, faible si c'est une passphrase humaine.
- **Correction** : exiger une clé haute entropie (ou HKDF/Argon2 + sel persistant).

### ⚠️ [Faible] Prod bootable avec stores in-memory
- **Fichiers** : `api/src/main.rs:102-108` (avertissement stderr, pas d'échec).
- **Impact** : révocations/permissions/audit non persistés si l'opérateur oublie
  `AUTOME_PERSISTENCE=postgres`. **Correction** : en `Prod`, échouer (fail-fast)
  plutôt qu'avertir.

### Absences notables
- ❌ **Pas de rate limiting / anti-bruteforce** sur l'API (login futur exposé).
- ⚠️ **Pas de CORS/headers de sécurité** explicites côté API (à valider).
- ⚠️ **Vérification de signature webhook** présente pour GitHub
  (`connectors/src/github.rs`, HMAC) mais **non retrouvée pour Slack**
  (`x-slack-signature`) — à confirmer/compléter.

### Points sécurité **positifs** (vérifiés)
- ✅ **Pas d'injection SQL** : 100 % des requêtes sont paramétrées (`$1,$2…`,
  `run_state.rs`, `workflow_repo.rs`) ; les `format!` ne concatènent que des
  **noms de colonnes constants** (`RUN_COLS`), jamais d'entrée utilisateur.
- ✅ **AEAD correct** : nonce frais par chiffrement via OsRng, tag GCM vérifié,
  tampering/clé erronée rejetés (tests `cipher.rs:224-279`).
- ✅ **JWT robuste** : signature + `exp` (horloge injectée), révocation par `jti`,
  rotation refresh **anti-rejeu** (`jwt.rs:143-169`, test `rotate_rejects_already_revoked`).
- ✅ **Secrets jamais exposés** : `Debug` redacté (`config.rs:53-58`), secrets prod
  obligatoires non-défaut (`config.rs:120-143`), `client_secret` jamais renvoyé
  (`ProviderInfo`/`CredentialSummary`, `credentials.rs:32-67`).
- ✅ **`#![forbid(unsafe_code)]`** au niveau workspace (`Cargo.toml`).
- ✅ **TLS** rustls all-or-nothing (`main.rs:118-144`) — pas de dégradation silencieuse.

---

# 9. Analyse des Tests — **Note : 8/10**

- ✅ **Volume & couverture apparente** : **529** fonctions de test backend
  réparties dans tous les crates + **23** fichiers de test frontend (Vitest/jsdom).
- ✅ **Unitaires** : crypto, jwt, rbac, config, mapping, condition couverts avec
  cas limites (envelope malformée, token altéré, rejet replay…).
- ✅ **Intégration** : `crates/api/tests/api_integration.rs` (axum + reqwest sur
  port éphémère), `tls.rs` (HTTPS réel), `persistence/tests/pg_*.rs`,
  `connectors/tests/connectors_integration.rs`.
- ✅ **E2E** : `crates/worker/tests/e2e_workflow.rs`, exemples exécutables
  `crates/engine/tests/example_*.rs`, et `frontend/src/e2e.test.tsx`.
- ✅ **Charge** : `crates/worker/tests/load.rs`.
- ✅ **Mocks/seams** : `MockClient` (`http.rs`), `FixedClock`/`MovableClock`,
  `RecordingClient` OAuth, stores in-memory — pratique de test mûre.
- ⚠️ **Ce qui manque** : tests d'autorisation **négatifs** par propriétaire
  (impossible aujourd'hui, cf. §8 BOLA) ; tests SSRF ; tests de la chaîne
  login→token (inexistante) ; pas de mesure de couverture chiffrée en CI
  (pas de `tarpaulin`/`llvm-cov`).
- ⚠️ Les tests PG nécessitent une base ; la CI tourne **in-memory** (`ci.yml`
  commente « pas de service externe ») → le chemin Postgres n'est pas exercé en CI.

---

# 10. Performance

- **Critique** : aucun problème bloquant identifié.
- **Important** :
  - ⚠️ **N requêtes par run** dans `PgRunStateStore::checkpoint` : `DELETE` puis
    une `INSERT` **par node** dans une boucle (`run_state.rs:132-139`). Pour des
    workflows larges, préférer un `INSERT ... UNNEST`/`COPY`. De même
    `row_to_checkpoint` fait une requête `run_steps` **par run** dans les listings
    (`list_by_workflow`, `list_orphans`) → **N+1** (`run_state.rs:159-234`).
  - ⚠️ `ReqwestHttpClient` lit `resp.text()` puis `serde_json::from_str` sans
    **borne de taille** (`reqwest_client.rs:66-72`) → risque d'allocation
    excessive sur grosse réponse.
- **Mineur** :
  - `fresh_jti` alloue via `format!` par octet (`jwt.rs:64-66`) — négligeable.
  - ✅ Profil release optimisé (`lto="thin"`, `codegen-units=1`, `strip`).

---

# 11. Scalabilité

Architecture **pensée pour scaler** : workers stateless, état en base, découplage
par file, backpressure (`BoundedQueue`, `worker/src/lib.rs:34-38`).

**Mais limites réelles tant que la dette #107 n'est pas soldée** : plusieurs
coordinateurs sont **in-process** — `InMemoryDedupLock` (`scheduler.rs:55`),
`InMemoryIdempotencyStore` (`worker/src/lib.rs:103`), et révocation/permissions
in-memory par défaut. Ces composants **cassent l'horizontalité**.

- **x10 utilisateurs** : OK si mono-instance API + quelques workers, mode Postgres.
- **x100** : nécessite plusieurs répliques API → la dédup/idempotence in-memory
  provoque des **double-déclenchements** ; tokens OAuth in-memory non partagés.
  Il faut PG/Redis pour ces verrous.
- **x1000** : exige aussi de régler le **N+1** persistance (§10), un store
  d'idempotence/dedup **distribué**, du partitionnement des runs, et le découpage
  scheduler en singleton élu (leader election) pour éviter les ticks concurrents.

✅ Le scheduler prévoit déjà l'extension (`DedupLock` trait, commentaires F5.4
Redis `SETNX`/PG advisory lock) — le chemin est tracé, pas encore implémenté.

---

# 12. DevOps

- ✅ **Docker** multi-stage, **non-root** (`useradd ... uid 10001`), images de base
  **pinnées par digest** (`docker/api.Dockerfile`), `--locked` pour la repro.
- ✅ **Docker Compose** complet (db, queue, api, worker, frontend, prometheus,
  grafana) avec healthchecks.
- ✅ **Kubernetes** : manifests Kustomize complets (`k8s/`: api, worker, frontend,
  postgres, rabbitmq, ingress, monitoring, namespace, config).
- ✅ **CI** (`.github/workflows/ci.yml`) : `fmt --check`, `clippy -D warnings`,
  `test --workspace --locked`, build release ; frontend typecheck/lint/test ;
  `concurrency` + `cancel-in-progress` ; `permissions: contents: read`.
- ✅ **CD** (`cd.yml`) : build+push GHCR par matrice, déclenché après CI verte,
  déploiement k8s (dry-run sans kubeconfig).
- ✅ **Observabilité infra** : `/metrics` Prometheus (`api/src/metrics.rs`),
  dashboards Grafana provisionnés.
- ⚠️ **Logging applicatif** : `eprintln!` au lieu de `tracing` structuré → pas de
  corrélation/niveaux/format JSON. **P1**.
- ⚠️ **Pas de tracing distribué** (OpenTelemetry) ni de health *readiness* vs
  *liveness* différenciés (`/health` unique, `main.rs:27`).
- ⚠️ **Incohérence compose** : `docker-compose.yml` fournit db+queue mais ne
  positionne **pas** `AUTOME_PERSISTENCE=postgres`/`AUTOME_QUEUE=rabbitmq` → la
  stack tourne en in-memory (commentaire d'en-tête désormais obsolète puisque le
  câblage PG/AMQP existe). **P2**.

---

# 13. Gestion de la Base de Données

- ✅ **Schéma** clair, **migrations versionnées** 0001→0009 + runner idempotent
  (`persistence/src/migrations.rs`), validables via `scripts/check-migrations.sh`.
- ✅ **Index** pertinents : FK (`idx_*_owner`, `idx_runs_workflow`), opérationnels
  (`idx_runs_status`, `created_at DESC`), audit (`actor`, `timestamp_ms`),
  révocations (`exp_ms` pour GC). Cf. `0005` et `0009`.
- ✅ **Contraintes** : `CHECK` sur statuts/rôles, `UNIQUE` métier, FK `ON DELETE
  CASCADE`, FK composite différable pour `current_version` (`0005`).
- ✅ **Transactions** : checkpoint des runs en transaction (`run_state.rs:91-141`).
- ❌ **Perte d'intégrité référentielle en 0009** : `runs` n'a **plus de FK** vers
  `workflows` (choix assumé « cohérent avec l'in-memory ») et `owner_id` retiré
  → conséquence directe du BOLA (§8). Risque : runs orphelins, pas de garde-fou DB.
- ⚠️ **N+1** sur la lecture des steps (§10).
- ⚠️ **0009 fait `DROP TABLE ... CASCADE`** : acceptable « base gérée par le
  runner, pas de données prod » (commentaire), mais **destructif** — à proscrire
  une fois des données réelles présentes ; prévoir des migrations additives.

---

# 14. Documentation — **Note : 8/10**

- ✅ **README** riche (objectif, stack, quickstart, structure, diagramme mermaid).
- ✅ **ADR** présents et pertinents (`docs/adr/0001-0004` : framework, migration,
  broker, crypto) — rare et précieux.
- ✅ **Docs dédiées** : `ARCHITECTURE.md`, `INSTALLATION.md`, `CONNECTORS.md`,
  `CICD.md`, `.env.example` très commenté.
- ✅ **Commentaires de code** d'excellente qualité (le « pourquoi », les frontières
  de feature, les liens vers les F*/ADR).
- ✅ `CONVENTIONS.md`, `ROADMAP.md` et le `CLAUDE.md` de dev ont été déplacés
  dans `claude-code/` ; les liens du README, de `docs/` (ADR inclus) et
  d'`examples/` pointent vers ce nouvel emplacement.
- ⚠️ Le README annonce « 56/66 features » et `docs` parlent de dette #107 :
  désynchronisation possible entre doc et état réel (ex. compose in-memory).

---

# 15. Dette Technique

| # | Problème | Fichiers | Impact | Priorité |
|---|---|---|---|---|
| 1 | Auth lifecycle (login/signup/refresh) non implémentée | `api/src/main.rs`, `core/src/auth/password.rs` | API inutilisable de bout en bout | **P0** |
| 2 | BOLA — pas d'owner/tenant sur workflows & runs | `workflows.rs`, `runs.rs`, `0009` | Fuite/altération inter-utilisateurs | **P0** |
| 3 | SSRF action HTTP / client reqwest | `http.rs`, `reqwest_client.rs` | Accès interne/métadonnées cloud | **P0/P1** |
| 4 | Tokens OAuth in-memory même en Postgres | `runtime.rs:37` | Connecteurs cassés en prod | **P1** |
| 5 | Stores dédup/idempotence/révocation in-memory | `scheduler.rs`, `worker/lib.rs` | Bloque le scaling horizontal | **P1** |
| 6 | Observabilité = `eprintln!` (pas de tracing) | `auth.rs`, `workflows.rs` | Exploitation difficile | **P1** |
| 7 | N+1 / inserts en boucle persistance runs | `run_state.rs` | Latence sur gros workflows | **P2** |
| 8 | Prod bootable en in-memory (warning seul) | `main.rs:102` | Risque opérateur | **P2** |
| 9 | Compose ne câble pas PG/AMQP | `docker-compose.yml` | Démo trompeuse | **P2** |
| 10 | Bornes de taille réseau / réponse absentes | `reqwest_client.rs` | DoS mémoire | **P2** |
| 11 | Docs supprimées encore référencées | `README.md` | Liens cassés | **P3** |
| 12 | Micro-duplications (`percent_encode`, hex) | `oauth/client.rs`, `jwt.rs` | Cosmétique | **P3** |

---

# 16. Refactoring Recommandé

### Quick wins (bénéfice élevé / effort faible / risque faible)
1. **Fail-fast prod en in-memory** (`main.rs:102`) : transformer le warning en
   erreur si `Profile::Prod`. Bénéfice sécurité, effort minime.
2. **Brancher le store OAuth PG** en mode Postgres (`runtime.rs:37`). Effort faible.
3. **Borne de taille de réponse** dans `ReqwestHttpClient`. Effort faible.
4. ~~Restaurer/mettre à jour `CONVENTIONS.md`/`ROADMAP.md` ou les liens README.~~ Fait (déplacés dans `claude-code/`).

### Moyen terme (1–4 semaines)
5. **Implémenter l'auth lifecycle** (#1) + tests e2e login→token→appel protégé.
6. **Filtrage SSRF** (#3) : module `safe_url` partagé (schéma + résolution IP +
   blocage loopback/link-local/privé) appliqué à `http.rs` et connecteurs.
7. **Réintroduire owner/tenant** (#2) : colonne, renseignement depuis `claims.sub`,
   filtrage systématique + 404, tests d'autz négatifs.
8. **Migrer `eprintln!` → `tracing`** + souscripteur JSON, niveaux, requ-id.

### Long terme (1–3 mois)
9. **Coordinateurs distribués** : dédup/idempotence/leader-election (Redis/PG)
   pour le scaling horizontal réel (#5).
10. **Cohérence dispatch↔checkpoint** : pattern Outbox (#manque §7).
11. **Optimiser la persistance des runs** (batch insert, jointure unique) (#7).
12. **Couverture chiffrée en CI** (`llvm-cov`) + job PG réel (testcontainers).

---

# 17. Checklist Production

| Environnement | Prêt ? | Justification |
|---|---|---|
| **Développement** | ✅ Oui | `docker compose up` ou `cargo run` ; DX excellente, tests verts. |
| **Staging (démo)** | ⚠️ Partiel | Fonctionne en mode Postgres, mais sans auth câblée et sans isolation utilisateur ; OK pour démontrer le moteur, pas le multi-utilisateur. |
| **Bêta** | ❌ Non | Bloqué par #1 (auth) et #2 (BOLA) : impossible d'ouvrir à plusieurs vrais utilisateurs en sécurité. |
| **Production** | ❌ Non | P0 #1/#2/#3 non résolus + scaling horizontal limité (#5) + observabilité insuffisante (#6). Les **fondations** sont solides, le **chemin** est clair. |

---

# 18. Roadmap d'Amélioration

**Phase 1 — Immédiat (sécurité bloquante)**
- Fail-fast prod in-memory ; store OAuth PG ; borne taille réponse.
- Implémenter `/auth/signup|login|refresh` (Argon2id + `TokenService`) — **#1 P0**.
- Filtrage SSRF partagé — **#3**.

**Phase 2 — 1 à 4 semaines (intégrité & exploitabilité)**
- Owner/tenant + autorisation par ressource + tests négatifs — **#2**.
- `tracing` structuré + readiness/liveness séparés — **#6**.
- Câbler compose sur PG/AMQP ; job CI avec PostgreSQL réel.

**Phase 3 — 1 à 3 mois (scalabilité)**
- Dédup/idempotence/révocation distribuées (Redis/PG) + leader election scheduler — **#5**.
- Optimisation persistance runs (batch) + Outbox dispatch — **#7**.
- Rate limiting / anti-bruteforce ; headers de sécurité ; vérif signature Slack.

**Phase 4 — 3 à 12 mois (industrialisation)**
- JWT RS256/EdDSA + rotation KMS ; HKDF pour la clé de chiffrement.
- OpenTelemetry (tracing distribué), SLO/alerting Grafana.
- Multi-tenant complet, quotas par tenant, partitionnement des runs, DR/backup.

---

# Score global

| Axe | Note /10 | Pondération | Points |
|---|---|---|---|
| Architecture | 9 | ×1.5 | 13.5 |
| Maintenabilité | 9 | ×1.5 | 13.5 |
| Sécurité | 5 | ×2.0 | 10.0 |
| Performance | 7 | ×1.0 | 7.0 |
| Tests | 8 | ×1.0 | 8.0 |
| Scalabilité | 6 | ×1.0 | 6.0 |
| Qualité du code | 9 | ×1.5 | 13.5 |
| Documentation | 8 | ×0.5 | 4.0 |
| **Total pondéré** | | **/100** | **75.5 / 100** |

> *Pondération : la sécurité compte double (criticité d'un orchestrateur exécutant
> du code réseau), architecture/maintenabilité/qualité ×1.5 (force du projet).*

## Verdict
**Autome est un projet d'ingénierie de très haut niveau (architecture, qualité,
tests, DevOps) dont le produit n'est pas encore terminé.** Les fondations
sécuritaires sont excellentes là où elles existent, mais **trois lacunes P0**
(auth non câblée, autorisation par ressource absente, SSRF) interdisent une mise
en production. Une fois la Phase 1 et la Phase 2 traitées (≈ 4–6 semaines), le
projet basculera d'une **excellente base technique** à un **produit déployable**.

**Score global : 75.5 / 100** — *« Très bonne base, non production-ready en l'état. »*
