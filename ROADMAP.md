# 🗺️ Roadmap & Suivi des Features — Autome

> Outil d'automatisation (type Make / n8n / Zapier) — backend Rust, frontend React/TS.
>
> **Légende du statut :**
> - `[ ]` À faire
> - `[~]` En cours
> - `[x]` Terminé
> - `[!]` Bloqué / à discuter
>
> Mettre à jour la case **et** la colonne *Statut* / *Date* du tableau récap à chaque avancée.

---

## 📊 Tableau récapitulatif

| #  | Domaine                  | Features | Terminées | Statut global |
|----|--------------------------|----------|-----------|---------------|
| 1  | Fondations & Architecture| 6        | 6         | ✅ Terminé    |
| 2  | Moteur de workflows      | 7        | 7         | ✅ Terminé    |
| 3  | Triggers & Actions       | 6        | 6         | ✅ Terminé    |
| 4  | Connecteurs API          | 5        | 5         | ✅ Terminé    |
| 5  | Orchestrateur & Scaling  | 6        | 6         | ✅ Terminé    |
| 6  | Sécurité & Auth          | 6        | 6         | ✅ Terminé    |
| 7  | API publique (REST/gRPC) | 4        | 4         | ✅ Terminé    |
| 8  | Frontend & Éditeur visuel| 7        | 7         | ✅ Terminé    |
| 9  | Tests (TDD)              | 5        | 1         | 🟡 En cours   |
| 10 | DevOps & Déploiement     | 6        | 0         | À faire       |
| 11 | Documentation            | 5        | 0         | À faire       |
| 12 | Exemples de workflows    | 3        | 0         | À faire       |

**Progression totale : 48 / 66 features** (≈ 150 sous-tâches sur les features détaillées)

> Les features complexes (F1.3, F1.5, F2.*, F4.*, F5.1–F5.4, F6.1–F6.3, F6.5,
> F8.2–F8.7) sont découpées en sous-tâches `Fx.y.z`. Une feature est `[x]`
> quand **toutes** ses sous-tâches le sont.

---

## 1. Fondations & Architecture

- [x] **F1.1** — Document d'architecture (diagrammes composants, flux de données, déploiement) — _[docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md), issue #1_
- [x] **F1.2** — Choix & justification du framework backend (axum vs actix-web) — _[docs/adr/0001-choix-framework-backend.md](./docs/adr/0001-choix-framework-backend.md), issue #3_
- [x] **F1.3** — Mise en place du workspace Cargo (crates : `core`, `engine`, `api`, `connectors`, `worker`) — _issue #5_
  - [x] F1.3.a — `Cargo.toml` workspace racine + résolution des versions
  - [x] F1.3.b — Crate `core` (types partagés, erreurs, traits)
  - [x] F1.3.c — Crate `engine` (moteur d'exécution)
  - [x] F1.3.d — Crate `api` (serveur HTTP/gRPC)
  - [x] F1.3.e — Crate `connectors` (intégrations tierces)
  - [x] F1.3.f — Crate `worker` (exécuteur de runs)
  - [x] F1.3.g — Lint/format partagés (`rustfmt.toml`, `clippy` en CI-ready)
- [x] **F1.4** — Modèle de données workflow (schéma JSON/struct : nodes, edges, params) — _issue #7_
- [x] **F1.5** — Schéma PostgreSQL initial + migrations (workflows, runs, credentials, users) — _issue #9_
  - [x] F1.5.a — Choix de l'outil de migration (sqlx-cli / refinery / diesel) — _ADR 0002 : sqlx_
  - [x] F1.5.b — Table `users`
  - [x] F1.5.c — Table `workflows` (+ versioning)
  - [x] F1.5.d — Table `runs` & `run_steps` (état d'exécution)
  - [x] F1.5.e — Table `credentials` (secrets chiffrés)
  - [x] F1.5.f — Index & contraintes (FK, unicité)
  - [x] F1.5.g — Seed de données de dev
- [x] **F1.6** — Configuration centralisée (env, secrets, profils dev/prod) — _issue #11_

## 2. Moteur de workflows

- [x] **F2.1** — Représentation du graphe de workflow (DAG, validation cycles) — _issue #13_
  - [x] F2.1.a — Structures `Node`, `Edge`, `Graph`
  - [x] F2.1.b — Parsing/désérialisation depuis le modèle workflow
  - [x] F2.1.c — Validation : détection de cycles
  - [x] F2.1.d — Validation : ports/connexions cohérents, node racine unique
- [x] **F2.2** — Exécution séquentielle des nodes — _issue #15_
  - [x] F2.2.a — Tri topologique
  - [x] F2.2.b — Boucle d'exécution + contexte de run
  - [x] F2.2.c — Collecte des résultats par node
- [x] **F2.3** — Branches conditionnelles (if / switch) — _issue #17_
  - [x] F2.3.a — Node `If` (true/false)
  - [x] F2.3.b — Node `Switch` (n branches)
  - [x] F2.3.c — Évaluation d'expression de condition
- [x] **F2.4** — Boucles (iterate sur collections, while) — _issue #19_
  - [x] F2.4.a — Node `Loop` (iterate sur collection)
  - [x] F2.4.b — Node `While` (condition + garde anti-boucle infinie)
  - [x] F2.4.c — Agrégation des sorties d'itérations
- [x] **F2.5** — Exécution parallèle (fan-out / fan-in) — _issue #21_
  - [x] F2.5.a — Fan-out (lancement concurrent des branches)
  - [x] F2.5.b — Fan-in (synchronisation / jointure des résultats)
  - [x] F2.5.c — Limite de concurrence configurable
- [x] **F2.6** — Passage & mapping de données entre nodes (expressions / templating) — _issue #23_
  - [x] F2.6.a — Moteur d'expressions (accès aux sorties précédentes)
  - [x] F2.6.b — Templating de chaînes (`{{ }}`)
  - [x] F2.6.c — Mapping/transformation des champs d'entrée
- [x] **F2.7** — Gestion d'erreurs : retry, fallback, timeout par node — _issue #25_
  - [x] F2.7.a — Politique de retry (nb essais, backoff)
  - [x] F2.7.b — Timeout par node
  - [x] F2.7.c — Branche/handler d'erreur (fallback)
  - [x] F2.7.d — Propagation & statut final du run

## 3. Triggers & Actions

- [x] **F3.1** — Trait/abstraction `Trigger` extensible — _issue #27_
- [x] **F3.2** — Trigger Webhook (HTTP entrant) — _issue #29_
- [x] **F3.3** — Trigger Scheduler (cron / intervalle) — _issue #31_
- [x] **F3.4** — Trigger Polling (vérification périodique d'une source) — _issue #33_
- [x] **F3.5** — Trait/abstraction `Action` extensible — _issue #35_
- [x] **F3.6** — Action HTTP générique (requête REST configurable) — _issue #37_

## 4. Connecteurs API

- [x] **F4.1** — Connecteur Slack (envoi message, lecture canal) — _issue #39_
  - [x] F4.1.a — Auth Slack (OAuth / bot token)
  - [x] F4.1.b — Action `send_message`
  - [x] F4.1.c — Action `read_channel` / lecture historique
  - [x] F4.1.d — Mapping erreurs API → erreurs moteur
- [x] **F4.2** — Connecteur Gmail (envoi / lecture mail) — _issue #41_
  - [x] F4.2.a — Auth Gmail (OAuth2 Google)
  - [x] F4.2.b — Action `send_email`
  - [x] F4.2.c — Action `list/read messages`
  - [x] F4.2.d — Gestion pièces jointes
- [x] **F4.3** — Connecteur Notion (création / lecture page) — _issue #43_
  - [x] F4.3.a — Auth Notion (token intégration)
  - [x] F4.3.b — Action `create_page`
  - [x] F4.3.c — Action `read_page` / query database
  - [x] F4.3.d — Conversion blocs Notion ↔ données workflow
- [x] **F4.4** — Connecteur GitHub (issues, PR, webhooks) — _issue #45_
  - [x] F4.4.a — Auth GitHub (PAT / GitHub App)
  - [x] F4.4.b — Actions issues (créer, commenter, fermer)
  - [x] F4.4.c — Actions PR (lister, créer, merger)
  - [x] F4.4.d — Réception webhooks GitHub (vérif signature)
- [x] **F4.5** — SDK interne pour créer un connecteur (doc + template) — _issue #47_
  - [x] F4.5.a — Trait `Connector` + cycle de vie auth
  - [x] F4.5.b — Helpers HTTP (rate-limit, retry, pagination)
  - [x] F4.5.c — Schéma de déclaration des actions/params
  - [x] F4.5.d — Template/exemple de connecteur + doc

## 5. Orchestrateur & Scaling

- [x] **F5.1** — File de messages (Kafka ou RabbitMQ) — choix + intégration — _issue #49_
  - [x] F5.1.a — Décision Kafka vs RabbitMQ (critères documentés)
  - [x] F5.1.b — Abstraction `Queue` (publish/consume)
  - [x] F5.1.c — Implémentation concrète + config connexion
  - [x] F5.1.d — Dead-letter queue
- [x] **F5.2** — Worker pool pour exécution distribuée des runs — _issue #51_
  - [x] F5.2.a — Consumer worker (pull depuis la file)
  - [x] F5.2.b — Pool de tâches concurrentes + arrêt propre
  - [x] F5.2.c — Ack/nack & idempotence
- [x] **F5.3** — Planificateur de jobs (dispatch triggers → file) — _issue #53_
  - [x] F5.3.a — Scheduler cron/intervalle
  - [x] F5.3.b — Dispatch trigger → message de run
  - [x] F5.3.c — Anti-double-déclenchement (lock distribué)
- [x] **F5.4** — Persistance de l'état d'exécution (reprise après crash) — _issue #55_
  - [x] F5.4.a — Checkpoint de l'état du run en base
  - [x] F5.4.b — Reprise d'un run interrompu
  - [x] F5.4.c — Nettoyage des runs orphelins
- [x] **F5.5** — Backpressure & limites de concurrence — _issue #57_
- [x] **F5.6** — Test de charge (objectif : milliers de workflows simultanés) — _issue #59_

## 6. Sécurité & Auth

- [x] **F6.1** — Authentification utilisateur (JWT) — _issue #61_
  - [x] F6.1.a — Signup / hash mot de passe (argon2)
  - [x] F6.1.b — Login → émission JWT (access + refresh)
  - [x] F6.1.c — Middleware de vérification du token
  - [x] F6.1.d — Révocation / rotation des tokens
- [x] **F6.2** — OAuth2 (login + autorisation connecteurs tiers) — _issue #71_
  - [x] F6.2.a — Flow Authorization Code (login)
  - [x] F6.2.b — Stockage & refresh des tokens connecteurs
  - [x] F6.2.c — Gestion multi-provider
- [x] **F6.3** — Chiffrement des credentials/secrets au repos — _issue #63_
  - [x] F6.3.a — Choix de la stratégie (AES-GCM + KMS / env key)
  - [x] F6.3.b — Chiffrement/déchiffrement transparent en base
  - [x] F6.3.c — Rotation de clé
- [x] **F6.4** — TLS sur tous les services — _issue #69_
- [x] **F6.5** — RBAC (rôles & permissions) — _issue #65_
  - [x] F6.5.a — Modèle rôles/permissions
  - [x] F6.5.b — Enforcement au niveau API (guards)
- [x] **F6.6** — Audit log des actions sensibles — _issue #67_

## 7. API publique (REST / gRPC)

- [x] **F7.1** — CRUD workflows (créer, lister, modifier, supprimer) — _issue #73_
  - [x] F7.1.a — `WorkflowRepository` trait + impl in-memory (`core::workflow::repo`)
  - [x] F7.1.b — Endpoints REST `GET/POST/PUT/DELETE /workflows[/{id}]`
  - [x] F7.1.c — RBAC `workflows.read|write` par route + audit log par appel
- [x] **F7.2** — Déclenchement manuel & suivi d'un run — _issue #75_
  - [x] F7.2.a — `RunDispatcher` trait + `QueueRunDispatcher` (persistance + publication)
  - [x] F7.2.b — `POST /workflows/:id/runs` (202 + `run_id`)
  - [x] F7.2.c — `GET /runs/:run_id` (404 si inconnu)
  - [x] F7.2.d — `GET /workflows/:id/runs` (pagination, plus récent d'abord)
  - [x] F7.2.e — RBAC `runs.execute|read` + audit log par route
- [x] **F7.3** — Endpoints credentials / connecteurs — _issue #77_
  - [x] F7.3.a — `CredentialService` (orchestration `OAuthClient` + `ConnectorTokenStore`)
  - [x] F7.3.b — REST `/credentials[/providers|/:provider/(authorize|callback)]`
  - [x] F7.3.c — RBAC `credentials.manage` + audit log par appel
- [x] **F7.4** — Spec OpenAPI générée (utoipa) — _issue #79_
  - [x] F7.4.a — Deps `utoipa` + `#[derive(ToSchema)]` sur tous les DTO publics
  - [x] F7.4.b — `#[utoipa::path(...)]` sur chaque handler (tag + responses + security)
  - [x] F7.4.c — `OpenApiDoc` racine + route `GET /openapi.json` + tests

## 8. Frontend & Éditeur visuel

- [x] **F8.1** — Setup React + TypeScript + TailwindCSS — _issue #81_
  - [x] F8.1.a — Bootstrap Vite + React 18 + TypeScript strict (`frontend/`)
  - [x] F8.1.b — TailwindCSS v4 + styles de base (palette neutre)
  - [x] F8.1.c — Skeleton SPA + helper fetch backend (`/openapi.json`)
  - [x] F8.1.d — Vitest + RTL + ESLint + scripts pnpm
- [x] **F8.2** — Canvas drag-and-drop (nodes + connexions) — _issue #83_
  - [x] F8.2.a — Choix de la lib (React Flow / xyflow)
  - [x] F8.2.b — Rendu des nodes + ajout par drag depuis la palette
  - [x] F8.2.c — Création/suppression de connexions
  - [x] F8.2.d — Zoom / pan / mini-map
  - [x] F8.2.e — Sérialisation du graphe ↔ modèle backend
- [x] **F8.3** — Palette de nodes (triggers, actions, logique) — _issue #85_
  - [x] F8.3.a — Catalogue des nodes (catégories + recherche)
  - [x] F8.3.b — Drag depuis la palette vers le canvas
  - [x] F8.3.c — Chargement dynamique depuis l'API (nodes dispo)
- [x] **F8.4** — Panneau de configuration d'un node — _issue #87_
  - [x] F8.4.a — Formulaire généré depuis le schéma de params
  - [x] F8.4.b — Validation des champs + valeurs par défaut
  - [x] F8.4.c — Éditeur d'expressions / mapping de données
  - [x] F8.4.d — Sélection de credentials/connecteur
- [x] **F8.5** — Visualisation des exécutions (statut, logs par node) — _issue #89_
  - [x] F8.5.a — Liste des runs (statut, durée, déclencheur)
  - [x] F8.5.b — Vue détail : statut par node sur le canvas
  - [x] F8.5.c — Logs & données d'entrée/sortie par node
  - [x] F8.5.d — Mise à jour temps réel (WS/SSE ou polling)
- [x] **F8.6** — Auth UI (login, gestion connecteurs) — _issue #91_
  - [x] F8.6.a — Pages login / signup
  - [x] F8.6.b — Gestion de session (stockage token, refresh)
  - [x] F8.6.c — Écran connexion/autorisation OAuth des connecteurs
- [x] **F8.7** — Intégration API backend — _issue #93_
  - [x] F8.7.a — Client API typé (généré depuis OpenAPI)
  - [x] F8.7.b — Couche data-fetching (cache, états loading/erreur)
  - [x] F8.7.c — Intercepteur auth (token + 401 → refresh)

## 9. Tests (TDD)

- [x] **F9.1** — Tests unitaires moteur de workflows — _issue #95_
- [ ] **F9.2** — Tests d'intégration connecteurs (avec mocks)
- [ ] **F9.3** — Tests d'intégration API
- [ ] **F9.4** — Tests end-to-end (workflow complet)
- [ ] **F9.5** — Tests frontend (composants + e2e)

## 10. DevOps & Déploiement

- [ ] **F10.1** — Dockerfiles (backend, worker, frontend)
- [ ] **F10.2** — docker-compose dev (db, queue, services)
- [ ] **F10.3** — Manifests Kubernetes
- [ ] **F10.4** — Pipeline CI (build, lint, test)
- [ ] **F10.5** — Pipeline CD (build images, déploiement)
- [ ] **F10.6** — Monitoring Prometheus + dashboards Grafana

## 11. Documentation

- [ ] **F11.1** — README principal
- [ ] **F11.2** — Guide d'installation / démarrage
- [ ] **F11.3** — Doc d'architecture (avec diagrammes)
- [ ] **F11.4** — Guide création de connecteur
- [ ] **F11.5** — Checklist CI/CD & reproductibilité

## 12. Exemples de workflows

- [ ] **F12.1** — Slack → Gmail → Notion
- [ ] **F12.2** — Webhook GitHub → notification Slack
- [ ] **F12.3** — Scheduler → fetch API → stockage

---

## 📝 Journal des avancées

| Date | Feature(s) | Description | Auteur |
|------|------------|-------------|--------|
| 2026-05-16 | — | Création du document de roadmap | — |
| 2026-05-16 | F1.1 | Document d'architecture (`docs/ARCHITECTURE.md`) — diagrammes composants/flux/déploiement, modèle de données, ADR initial | — |
| 2026-05-16 | F1.2 | ADR 0001 : choix du framework backend → **axum** (stack Tokio/`tower` homogène, middlewares mutualisés API↔worker) — `docs/adr/0001-choix-framework-backend.md` | — |
| 2026-05-16 | F1.3 | Workspace Cargo : crates `core`/`connectors`/`engine`/`api`/`worker` (graphe de deps ARCHITECTURE §9), `rustfmt.toml` + lints workspace (`clippy -D warnings`). `build`+`test`+`clippy` verts. | — |
| 2026-05-16 | F1.4 | Modèle workflow dans `core` : `WorkflowGraph`/`Node`/`Edge`/`NodeKind`/`WorkflowId`, JSON serde stable + helpers (lookup, adjacence, roots). 6 tests verts. Validation graphe → F2.1. | — |
| 2026-05-16 | F1.5 | Schéma PostgreSQL : ADR 0002 (sqlx), migrations `0001..0005` (users, workflows+versioning, runs/run_steps, credentials, index/FK différable), seed dev. `scripts/check-migrations.sh` (Postgres jetable) vert. | — |
| 2026-05-16 | F1.6 | Config centralisée dans `core` : `Config`/`Profile`/`Secret` (env, défauts par profil, secrets redactés, validation fail-fast prod), câblée dans `api`/`worker`, `.env.example`. 12 tests core verts. **Domaine 1 (Fondations) terminé.** | — |
| 2026-05-16 | F2.1 | `engine::graph` : `Dag::from_workflow` valide unicité nodes, intégrité arêtes, ports par type (if/switch/...), racine unique, **acyclicité** (DFS tricolore). `GraphError` typé. 12 tests engine verts. Tri topo → F2.2. | — |
| 2026-05-16 | F2.2 | `engine::exec` : tri topologique Kahn déterministe, `RunContext` (sorties par node), `NodeRunner` injectable, `run_sequential` fail-fast → `RunReport`. `Engine::run` câblé (worker MAJ). 16 tests engine verts. | — |
| 2026-05-16 | F2.3 | Branches conditionnelles : `engine::condition` (opérandes, comparaisons, chemins de contexte), nodes `If`/`Switch` évalués par le moteur, `run_sequential` branch-aware (branches non prises → `Skipped`). 23 tests engine verts. | — |
| 2026-05-17 | F2.4 | Boucles : nodes `Loop` (itère `params.items`) / `While` (`condition` + garde `max_iterations`) évalués par le moteur, corps délégué au runner par itération (item/index exposés au contexte), sortie agrégée en tableau. 28 tests engine verts. | — |
| 2026-05-17 | F2.5 | Exécution parallèle : `run_parallel` (ordonnanceur ready-queue à threads scoped) — fan-out concurrent, fan-in par comptage d'arêtes, limite `max_concurrency`, sémantique identique au séquentiel, rapport stable. 33 tests engine verts. | — |
| 2026-05-17 | F2.6 | Mapping de données : `engine::mapping` (expressions via `condition::resolve`, templating `{{ }}` multi-occurrences, type préservé si template unique, résolution récursive des params). Injecté avant le runner (nodes de contrôle inchangés). 38 tests engine verts. | — |
| 2026-05-17 | F2.7 | Gestion d'erreurs : politique `params._error` (retry+backoff, timeout, on_error fail/continue/fallback), port `error`, fail-fast vs échec géré, statut final cohérent — séquentiel **et** parallèle. 44 tests engine verts. **Domaine 2 (Moteur) terminé.** | — |
| 2026-05-17 | F3.1 | Abstraction `Trigger` : `core::trigger` (`TriggerEvent`, trait `Trigger` pull `kind()`/`poll()`), agnostique réseau/horloge. 14 tests core verts. | — |
| 2026-05-17 | F3.2 | Trigger Webhook : `connectors::WebhookTrigger` (match de chemin, file FIFO, impl `Trigger`). Ingress HTTP réel câblé plus tard (domaine 7/8). 3 tests connectors verts. | — |
| 2026-05-17 | F3.3 | Trigger Scheduler : `ScheduleTrigger` (intervalle, horloge injectée) + `CronSchedule` (parseur 5 champs `*`/n/`*/step`/listes + `matches`). 7 tests connectors verts. | — |
| 2026-05-17 | F3.4 | Trigger Polling : `connectors::PollingTrigger` (source injectée, déduplication par champ-clé, émission FIFO des inédits). 9 tests connectors verts. | — |
| 2026-05-17 | F3.5 | Abstraction `Action` : `core::action` (trait `Action` `kind()`/`execute(params)->Result<Value>`), agnostique. 16 tests core verts. | — |
| 2026-05-18 | F3.6 | Action HTTP générique : `connectors::HttpAction` (params→`HttpRequest`, client `HttpClient` injecté, réponse→`{status,body}`), impl `core::Action`. 13 tests connectors verts. **Domaine 3 (Triggers & Actions) terminé.** | — |
| 2026-05-20 | F4.1 | Connecteur Slack : `SlackAuth::Bot` (en-têtes `Authorization: Bearer …` + JSON), actions `SlackSendMessage` (`chat.postMessage` POST) et `SlackReadChannel` (`conversations.history` GET, query pourcent-encodée), mapping `{ok:false,error}`/HTTP non-2xx/transport → `Error::Invalid`. Transport injecté via `HttpClient` (aucune dépendance réseau). 13 tests connectors verts. | — |
| 2026-05-20 | F4.2 | Connecteur Gmail : `GmailAuth::OAuth2`, actions `GmailSendEmail` (`users.messages.send` POST, RFC 822 → base64url sans padding, pièces jointes en `multipart/mixed`), `GmailListMessages` (`users.messages.list` GET, query `q`/`maxResults`/`pageToken`/`labelIds` répété), `GmailReadMessage` (`users.messages.get`) et `GmailGetAttachment` (`users.messages.attachments.get`). Mapping HTTP non-2xx → `Error::Invalid` avec `error.message` Google extrait. Transport via `HttpClient` (aucune dépendance réseau ajoutée). 20 nouveaux tests connectors (46 au total). | — |
| 2026-05-20 | F4.3 | Connecteur Notion : `NotionAuth::Integration` + `Notion-Version: 2022-06-28` figée. Actions `NotionCreatePage` (`POST /v1/pages`, `parent`/`properties` requis, `children`/`icon`/`cover` optionnels), `NotionReadPage` (`GET /v1/pages/{id}`, id pourcent-encodé), `NotionQueryDatabase` (`POST /v1/databases/{id}/query`, `filter`/`sorts`/`page_size`/`start_cursor`). Helpers blocs (F4.3.d) : `text_block`, `heading_block` (niveau clampé 1..=3), `extract_plain_text` (walk récursif des `plain_text` rich text). Mapping HTTP non-2xx → `Error::Invalid("notion: http <status> — <message> (<code>)")`. 17 nouveaux tests connectors (63 au total). | — |
| 2026-05-20 | F4.4 | Connecteur GitHub : `GithubAuth::Token` (PAT / installation token) + en-têtes obligatoires (`User-Agent`, `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`). 6 actions issues+PR : `GithubCreateIssue`/`CommentIssue`/`CloseIssue` (POST/POST/PATCH) et `GithubListPulls`/`CreatePull`/`MergePull` (GET/POST/PUT, owner/repo pourcent-encodés). F4.4.d `verify_github_signature(secret, body, header)` HMAC-SHA256 (`hmac`+`sha2`+`hex` ajoutés au workspace), comparaison constant-time via `Mac::verify_slice`, rejette préfixe absent / hex invalide / longueur ≠ 32 / secret vide / body altéré (vecteur officiel GitHub Webhooks vérifié). 22 nouveaux tests connectors (85 au total). | — |
| 2026-05-20 | F4.5 | SDK interne connecteur (`crate::sdk`) : `Credentials` (apply en-têtes), `ResponseInterpreter`+`interpret` (mutualise HTTP→`Error::Invalid`), `percent_encode`/`to_query`, `RetryingClient`+`RetryPolicy`+`Clock` injectable (backoff exponentiel plafonné, statuts retryables), `paginate_all` via closure « next », `ActionSchema`/`ParamSpec`/`ParamKind` sérialisables, `Connector` (provider+schemas), `RecordingClient` (factice partagé, mode sticky ou queue FIFO). **Refactor des 4 connecteurs** Slack/Gmail/Notion/GitHub sous le SDK — zéro duplication restante. Template `crate::template` (connecteur `echo` ~120 l. tests inclus) prouve l'usage de bout en bout. `serde` ajouté à connectors pour serializer les schémas. 184 tests verts au total (+35 vs F4.4). **Domaine 4 (Connecteurs API) terminé.** | — |
| 2026-05-21 | F5.1 | File de messages : ADR 0003 (RabbitMQ vs Kafka vs NATS → **RabbitMQ**, alignement task-queue, ops, throughput cible). `core::queue` agnostique du broker : trait `Queue` (`publish`/`poll`/`ack`/`nack`/`depth`), types `Message`/`Delivery`/`DeliveryId`/`Nack { Requeue, DeadLetter }`. `InMemoryQueue` thread-safe (Mutex+VecDeque, in-flight tracking, redeliveries++ sur requeue). DLQ : convention `dlq_of(q) = "{q}.dlq"`, `Nack::DeadLetter` republie en préservant `redeliveries`. Client `lapin` réel branché plus tard avec F5.2. 15 nouveaux tests core (31 au total, 199 workspace). | — |
| 2026-05-21 | F5.2 | Worker pool : `worker::lib` (nouveau) — trait `Handler` (`handle(&Delivery) -> HandlerVerdict { Ok/Retry/DeadLetter }`, impl blanket pour les closures), trait `IdempotencyStore` + `InMemoryIdempotencyStore` (HashSet sous Mutex, dédup via header `idempotency-key`), `WorkerPool::process_one()` (unité atomique : poll → handler → ack/nack selon verdict), `WorkerPool::start()` spawne N threads (`std::thread` + `Arc<AtomicBool>` pour le shutdown, pas de tokio), `RunningPool::stop()` signale + join. Drop safety (warning si oubli). 12 nouveaux tests worker (211 workspace). Le client `lapin` réel viendra dans une couche d'adaptation quand on aura besoin d'un vrai broker. | — |
| 2026-05-21 | F5.3 | Planificateur de jobs : `core::scheduler` — `Scheduler::tick()` (poll tous les triggers, dispatche les events neufs sur la queue) ; `Scheduler::start(interval)` spawne un thread de fond, `RunningScheduler::stop()` signale + join. Dispatch (F5.3.b) : `TriggerEvent` → `Message { source, payload }` avec headers `idempotency-key` + `trigger-source` — s'aligne avec la dédup côté worker (F5.2.c). Anti-double-déclenchement (F5.3.c) : trait `DedupLock` + `InMemoryDedupLock` (HashSet/Mutex), fonction de clé pluggable (`with_dedup_key`, défaut `"<source>::<json(payload)>"`). La vraie version cross-instance (Redis SETNX / PG advisory lock) arrive avec F5.4. **Chaîne trigger → queue → worker bouclée**. 11 nouveaux tests core (42 au total, 222 workspace). | — |
| 2026-05-21 | F5.4 | Persistance d'état d'exécution : `core::run_state` — `RunCheckpoint { run_id, workflow_id, version, status, started_at_ms, finished_at_ms, outputs (node→Value), next_node, error }`, trait `RunStateStore` (`checkpoint`/`load`/`list_orphans`/`mark_failed`), impl `InMemoryRunStateStore` thread-safe (Mutex+HashMap). F5.4.b reprise : `load(run_id)` renvoie le snapshot complet pour qu'un worker recrée son `RunContext` et continue à partir de `next_node`. F5.4.c orphelins : `list_orphans(older_than_ms)` filtre `status=Running` AND `started_at < cutoff` (strict), `mark_failed` idempotent. **`RunStatus` déplacé depuis `engine` vers `core`** (+ serde derives) ; `autome_engine::RunStatus` re-exporté pour rétrocompat — 0 régression sur les 44 tests engine. Le store PostgreSQL/sqlx contre les tables F1.5 (`runs`/`run_steps`) sera branché dans une PR suivante. 14 nouveaux tests core (56 au total, 236 workspace). | — |
| 2026-05-21 | F5.5 | Backpressure & limites de concurrence : nouveau variant `Error::Backpressure { queue, depth, max }` typé (F5.5.a) — distinct de `Invalid` pour que le caller puisse réagir (backoff, dégrader). `BoundedQueue<Q: Queue>` wrapper (F5.5.b) avec `with_limit(name, max)` (par queue) et `with_default_limit(max)` (fallback) ; `publish` rejette à la limite, `poll`/`ack`/`nack`/`depth` pass-through. Sémantique soft-bound (race depth/publish acceptable pour un seuil de protection ; `nack(Requeue)` interne n'est pas plafonné). Documentation worker (F5.5.c) : `WorkerPool.concurrency` = cap d'exécution concurrente par queue ; limites par workflow/tenant reportées à F6.5 RBAC. 10 nouveaux tests core (66 au total, 246 workspace). | — |
| 2026-05-21 | F5.6 | Test de charge end-to-end : `crates/worker/tests/load.rs` — 3 tests qui valident l'assemblage F5.1↔F5.5 sur la stack in-memory. (a) Smoke (run par défaut) — Scheduler+BoundedQueue+WorkerPool 2t sur 100 events, vérifie 0 perte / 0 DLQ / ≥ 500 msg/s. (b) Load (run sur `--ignored --nocapture`) — 10 000 events, 8 workers, mesure publish + consume throughput, assertion ≥ 1 000/s (mesure dev locale **97k publish / 54k consume**). (c) Garde-fou backpressure : burst 1 000 events sur queue limitée à 100 → exactement 100 acceptés + 900 `Error::Backpressure`. Documentation méthode + commande + ordres de grandeur in-source. **Domaine 5 (Orchestrateur & Scaling) terminé.** | — |
| 2026-05-21 | F6.1 | Authentification utilisateur (JWT) : module `core::auth` — F6.1.a `PasswordHasher` trait + `Argon2PasswordHasher` (Argon2id via crates `argon2` + `password-hash` + `rand`, format PHC, sel généré par OsRng, paramètres par défaut). F6.1.b/d `TokenService` HS256 (crate `jsonwebtoken`) + `Claims { sub, iat, exp, jti, kind: Access\|Refresh }` + `issue_access`/`issue_refresh`/`verify`/`rotate` (horloge `now` injectée, déterministe). F6.1.d `RevocationStore` trait + `InMemoryRevocationStore` (HashMap<jti, exp_ms> sous Mutex, GC opportuniste, idempotent sur max(exp)). F6.1.c `api::auth` (nouveau `api::lib`) — extractor axum `AuthenticatedUser`, état `AuthState` (token_service + revocation + Clock injectable), `AuthError` typé mappé vers 401 ; `protected_router("/whoami")` câblé via `Router::nest("/auth", …)` dans `main`. 22 nouveaux tests core (hash/verify, JWT roundtrip, tampering, expired, rotation, revocation) + 7 tests api (200 valide, 401 missing/malformed/expired/tampered/revoked, helper `user_id`). 277 tests workspace. Nouvelles deps : `jsonwebtoken`, `argon2`, `password-hash`, `rand` (+ `tower` et `http-body-util` en dev-deps `api`). | — |
| 2026-05-21 | F6.3 | Chiffrement des credentials/secrets au repos : ADR 0004 (AES-256-GCM + `KeyProvider` versionné). `core::secrets` — F6.3.b trait `KeyProvider` (`get(key_id)`/`current()`) + impl `InMemoryKeyProvider` (HashMap sous Mutex, `add_key/set_current/remove_key`, anti-collision de `key_id`). `EnvelopeCipher::encrypt(plaintext)/decrypt(envelope)` AES-256-GCM via crate `aes-gcm` (RustCrypto), nonces 96 bits aléatoires (OsRng), tag GCM 128 bits, format envelope versionné `v1:<key_id>:<nonce_b64>:<ct+tag_b64>` (parse strict, rejet de `v2` non supporté). F6.3.c rotation : `add_key(id, key, make_current=true)` ajoute une nouvelle clé tout en gardant les anciennes accessibles ; `EnvelopeCipher::reencrypt(env)` décode avec la clé d'origine et ré-encode avec `current()`, no-op si déjà à jour. Suppression d'une clé refusée si c'est la courante. 25 nouveaux tests core (roundtrip/binary/empty, nonces différents par appel, tampering détecté par tag GCM, mauvaise clé rejetée, envelope mal formée, key_id avec `:` refusé, rotation lit ancien et écrit nouveau, reencrypt déplace puis no-op puis fail après remove, Send+Sync). 302 tests workspace. Nouvelles deps : `aes-gcm`, `base64`. | — |
| 2026-05-21 | F6.5 | RBAC : `core::rbac` (F6.5.a) — `Permission` newtype + constantes (`PERM_WORKFLOWS_READ`/`WRITE`, `PERM_RUNS_EXECUTE`/`READ`, `PERM_CREDENTIALS_MANAGE`, `PERM_USERS_ADMIN`, `PERM_WILDCARD`), `Role` (nom + permissions, builders `super_admin`/`editor`/`viewer`), trait `PermissionStore::permissions_for(user_id)`, impl `InMemoryPermissionStore` (HashMap sous Mutex, `assign_role`/`grant`/`revoke`). `has_permission(set, needed)` honore le wildcard `"*"`. (F6.5.b) Extension `api::auth` : `AuthState` reçoit un `Arc<dyn PermissionStore>` ; méthode `AuthState::require_permission(user, perm) -> Result<(), AuthError>` ; nouveau variant `AuthError::Forbidden(perm)` → **403** (corps `"forbidden: missing '<perm>'"`). Route démo `/auth/admin` protégée par `PERM_USERS_ADMIN`. 14 nouveaux tests core (Permission roundtrip, has_permission positive/négative/wildcard, role builders, store assign/grant/revoke/idempotent/empty) + 5 nouveaux tests api (admin OK avec super_admin, **403 avec viewer**, 403 sans aucun rôle, 401 si token absent avant check perm, OK avec grant individuel). 321 tests workspace. | — |
| 2026-05-21 | F6.6 | Audit log : `core::audit` (F6.6.a/b) — `AuditEvent { timestamp_ms, actor, action, resource, outcome, metadata }` sérialisable JSON (snake_case outcome), `AuditOutcome { Success, Failure, Denied }` (la distinction `Failure`=technique vs `Denied`=refus auth évite de noyer les 403 légitimes dans le bruit des 500), trait `AuditSink` (append-only, `record`/`list`), impl `InMemoryAuditSink` (Mutex+Vec, ordre FIFO d'insertion) + impl `NoopAuditSink` (drops events, pour désactiver). (F6.6.c) Intégration `api::auth` : `AuthState` reçoit un `Arc<dyn AuditSink>` (toujours présent, possiblement `Noop`) ; `AuthState::require_permission` enregistre systématiquement un event `auth.permission_check` (`Success` ou `Denied`, actor=user_id, resource=perm demandée, timestamp issu du `Clock`). Une erreur d'audit n'invalide pas la décision (log stderr + continue). 12 nouveaux tests (8 core : event builder/JSON roundtrip/outcomes serialization, sink FIFO/snapshot/Send+Sync/Noop drop ; 4 api : record success+denied audit, pas d'event si 401 token absent, accumulation en ordre sur plusieurs appels). 333 tests workspace. | — |
| 2026-05-22 | F6.4 | TLS sur l'API (HTTPS) : F6.4.a `api::tls` — `load_rustls_config(cert_path, key_path) -> RustlsConfig` qui lit cert chain + clé privée PEM (PKCS#1/8/SEC1 acceptés via `rustls-pemfile`), installe le `CryptoProvider` ring de rustls de façon idempotente via `OnceLock` (workspace `rustls` configuré `default-features = false, features = ["ring", "std", "tls12", "logging"]` — pas de C deps via aws-lc-rs). F6.4.b wiring `main` : si `AUTOME_TLS_CERT` + `AUTOME_TLS_KEY` sont positionnées, bind `axum_server::bind_rustls` ; sinon HTTP en clair ; positionnement partiel (un seul des deux) → erreur explicite (pas de fallback silencieux). F6.4.c test d'intégration `tests/tls.rs` : génère cert self-signed via `rcgen`, démarre un mini serveur HTTPS sur port éphémère, GET `/health` via `reqwest` (cert ajouté en root) → 200 OK ; **+** test négatif sans trust → erreur TLS rejet. Nouvelles deps : `rustls` + `rustls-pemfile` (workspace), `axum-server` (api), `rcgen` + `reqwest` + `tempfile` (api dev-deps). 11 nouveaux tests (7 unit loader + 2 integration HTTPS + 2 main bin checks). 344 tests workspace. | — |
| 2026-05-23 | F6.2 | OAuth2 Authorization Code + PKCE (connecteurs tiers) : `connectors::oauth` — F6.2.c `OAuthProviderConfig` + `OAuthRegistry` (HashMap, `register`/`get`/`list`, écrase OK sur rotation client_secret) ; F6.2.a PKCE S256 (`generate_pkce`: verifier 32 octets aléatoires base64url, challenge = `base64url(sha256(verifier))`, vérifié contre vecteur FIPS SHA-256("abc")) + `generate_state` (CSRF) ; `OAuthClient<C: HttpClient>::start_authorization` construit l'URL d'autorisation (client_id/redirect_uri/response_type=code/scope/state/code_challenge/code_challenge_method=S256, ordre fixe), `exchange_code` POST form-urlencoded `application/x-www-form-urlencoded` au token endpoint (grant_type=authorization_code, parse `TokenSet { access_token, refresh_token?, expires_at_ms (= now+expires_in*1000), token_type (défaut "Bearer"), scope? }`), `refresh` POST grant_type=refresh_token (ne fabrique pas de refresh_token si le provider l'omet). F6.2.b `ConnectorTokenStore` trait + impl `InMemoryConnectorTokenStore<K: KeyProvider>` — sérialise `TokenSet` en JSON puis chiffre via `EnvelopeCipher` (F6.3), test explicite que le token en clair **n'apparaît pas** au repos. Nouvelles deps connectors : `rand` + `base64`. 31 nouveaux tests (6 pkce + 5 provider + 14 client + 7 store). 375 tests workspace. **Domaine 6 (Sécurité & Auth) terminé — 6/6.** | — |
| 2026-05-23 | F7.1 | CRUD workflows (REST API) : F7.1.a `core::workflow::repo` — `WorkflowRepository` trait (`create`/`get`/`list`/`update`/`delete`) + `Pagination` + `WorkflowPage { items, total }` + impl `InMemoryWorkflowRepository` (Mutex<HashMap>, tri stable par `WorkflowId`, idempotent delete). Le module `workflow.rs` est converti en répertoire `workflow/` avec sous-module `repo`. F7.1.b `api::workflows` — sous-router `/workflows` (`GET /`, `POST /`, `GET /:id`, `PUT /:id`, `DELETE /:id`) avec `WorkflowsState { auth, repo }`. Le `FromRequestParts` de `AuthenticatedUser` est rendu générique sur `S` où `AuthState: FromRef<S>` — permet de l'utiliser dans n'importe quel router qui expose un `AuthState` via `FromRef`. Mapping erreurs : `Conflict` → 409, `NotFound` → 404, `BadRequest` (id path ≠ body) → 400, autres → 500. F7.1.c — RBAC : chaque route appelle `state.auth.require_permission(&user, PERM_WORKFLOWS_READ\|WRITE)?` avant le repo (un viewer fait 403 sur POST/PUT/DELETE, un caller sans token 401). Audit : un event `workflows.<verb>` est émis par appel (actor = user_id, resource = workflow_id ou `"*"` pour list, outcome `Success`/`Failure`). 20 nouveaux tests (11 repo core + 14 api dont nominal CRUD, conflits 409/404/400, 401/403 RBAC, audit observable). Workspace **vert** : 152 core + 40 api + 146 (engine) + 44 (connectors) + 12 worker + integration. | — |
| 2026-05-23 | F7.2 | Déclenchement manuel & suivi d'un run : F7.2.a `core::run_dispatch` — trait `RunDispatcher::dispatch(DispatchRequest) -> Result<DispatchOutcome>` + impl `QueueRunDispatcher` qui (1) persiste un `RunCheckpoint { status: Queued }` via le `RunStateStore` puis (2) publie un `Message { source: "manual", headers { idempotency-key=run_id, trigger-source=manual } }` sur la queue cible (par défaut `"runs"`). Générateur d'`id` injectable (`with_id_generator`) — par défaut `format!("run-{:016x}", AtomicU64++)`, sera remplacé par `uuid::Uuid::v4()` quand la branche PG sera branchée. F7.2.d — extension `RunStateStore::list_by_workflow(workflow_id, Pagination) -> RunListPage` (tri `created_at_ms` desc, stabilisé par `run_id`). F7.2.b/c `api::runs` — sous-router avec `RunsState { auth, workflows, runs, dispatcher }` + `FromRef<RunsState> for AuthState` : `POST /workflows/:id/runs` (202 Accepted, body `{run_id, status:"queued"}`, 404 si workflow inconnu), `GET /runs/:run_id` (200 ou 404), `GET /workflows/:id/runs?offset=&limit=` (200, items les plus récents en premier). F7.2.e — RBAC : `runs.execute` requis sur POST, `runs.read` sur les GET (un viewer fait 403 sur POST, un caller sans token 401). Audit : event `runs.create|get|list` par appel (actor = user_id, resource = run_id ou workflow_id, outcome reflète la réussite vs 404). 25 nouveaux tests (6 dispatcher + 4 store list + 14 api + 1 doc). Workspace : **54 + 157 + 152 + 44 + 12 + integration**, tous verts. | — |
| 2026-05-24 | F7.3 | Endpoints credentials / connecteurs : F7.3.a `connectors::credentials` — trait `CredentialService` (object-safe : `list_providers`, `start_authorize`, `complete_authorize`, `list_for_user`, `delete`) + impl `OAuthCredentialService<C: HttpClient>` qui orchestre `OAuthClient` (F6.2.a) + `ConnectorTokenStore` (F6.2.b chiffré F6.3) ; types publics `ProviderInfo { provider_id, scopes }` et `CredentialSummary { provider_id, has_refresh_token, scope, expires_at_ms }` — **jamais** de `client_secret` ni de token brut. Extension `ConnectorTokenStore::list_by_user(user_id) -> Vec<provider_id>` (tri lexical) pour énumération. F7.3.b `api::credentials` — sous-router `/credentials` avec 5 endpoints : `GET /providers` (liste safe), `POST /:provider/authorize` (PKCE côté SPA : retourne `authorize_url + state + code_verifier`), `POST /:provider/callback` (échange code → tokens chiffrés), `GET /` (résumé pour le user courant), `DELETE /:provider` (révocation). Mapping erreurs : `NotFound` 404 (provider inconnu), `Upstream` 502 (échec OAuth), `Auth` 401/403, `Internal` 500. F7.3.c — RBAC : chaque route exige `PERM_CREDENTIALS_MANAGE` (viewer 403, editor/super_admin OK) + audit `credentials.<verb>` par appel (actor = user_id, resource = provider_id ou `"*"` pour les listes, outcome reflète Success/Failure). Nouvelle dep directe `autome-api → autome-connectors` (existait déjà transitivement via `engine`). 27 nouveaux tests (2 store list_by_user + 11 service connectors + 14 api dont 401/403/404/502/audit). Workspace : **165 connectors + 157 core + 68 api + 44 engine + 12 worker + integration**, tous verts. | — |
| 2026-05-26 | F7.4 | Spec OpenAPI générée (utoipa) : F7.4.a ajout dep workspace `utoipa = { version = "5", features = ["axum_extras"] }` + dep directe sur `autome-api` ; `#[derive(ToSchema)]` sur tous les DTO publics de l'API (workflows `ListResponse`, runs `CreateRunBody`/`CreateRunResponse`/`ListRunsResponse`, credentials `ProviderDto`/`ListProvidersResponse`/`AuthorizeResponse`/`CallbackBody`/`CredentialDto`/`ListCredentialsResponse`) ; `IntoParams` sur les `ListQuery` ; pour les types provenant de `core` non annotés (`WorkflowGraph`, `RunCheckpoint`), schema marqué `value_type = Vec<Object>` (le détail vit en JSON Schema libre — `core` n'est pas pollué par utoipa). F7.4.b — `#[utoipa::path(...)]` sur les 13 handlers (5 workflows + 3 runs + 5 credentials) avec method, tag, params, request_body, responses (avec codes 200/201/202/204/400/401/403/404/409/502 selon l'endpoint), security(("bearer" = [])). F7.4.c `api::openapi` — struct `ApiDoc` `#[derive(OpenApi)]` qui agrège paths + components(schemas) + tags(`workflows`/`runs`/`credentials`) ; `Modify` impl ajoute le scheme `bearer` (HTTP Bearer JWT) appliqué par chaque path. Sous-router exposant `GET /openapi.json` → `Json<utoipa::openapi::OpenApi>`. 6 nouveaux tests (HTTP 200 + valid OpenAPI 3 + chaque path enregistré + bearer scheme déclaré + au moins un endpoint référence bearer + sentinelle anti-fuite : aucun champ `client_secret`/`access_token`/`refresh_token` dans la spec sérialisée + tags workflows/runs/credentials présents). Workspace : **74 api + 165 connectors + 157 core + 44 engine + 12 worker + integration**, tous verts. **Domaine 7 (API publique) terminé — 4/4.** | — |
| 2026-05-30 | F8.1 | Setup React + TypeScript + TailwindCSS : bootstrap du frontend dans `frontend/` (monorepo Rust + JS, pnpm 11). F8.1.a Vite 5 + React 18 + TypeScript 5 strict (`strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`) ; `tsconfig.app.json` pour `src/`, `tsconfig.node.json` pour `vite.config.ts`. F8.1.b TailwindCSS v4 via `@tailwindcss/vite` (pas de PostCSS config nécessaire) + `@theme` tokens dans `src/index.css` (palette brand + neutral). F8.1.c skeleton SPA — `App.tsx` rend une page d'accueil qui consomme `GET /openapi.json` du backend (F7.4) comme canari d'intégration ; helper `src/api.ts` (`apiBaseUrl()` lit `VITE_API_URL`, `fetchOpenApi()` avec erreur typée `ApiError`, jamais de token en clair). F8.1.d outillage qualité — Vitest 2 + jsdom + `@testing-library/react` + `jest-dom` matchers ; ESLint 9 flat config (`typescript-eslint`, `react-hooks`, `react-refresh`) ; scripts pnpm `dev`/`build`/`preview`/`typecheck`/`lint`/`test`. 6 tests verts (4 `api.test.ts` : base URL fallback, parse 200, ApiError 5xx, ApiError transport ; 2 `App.test.tsx` : succès affiche nb d'endpoints, erreur affiche alerte). Lint, typecheck et `pnpm build` (144 kB JS gzip 47 kB) verts. `pnpm-workspace.yaml` autorise explicitement le post-install d'`esbuild` (pnpm 11 bloque par défaut). | — |
| 2026-05-30 | F8.2 | Canvas drag-and-drop : F8.2.a dep `@xyflow/react 12` (React Flow renommé) — choix retenu pour son support natif `nodes/edges/handles/zoom/pan/minimap`. F8.2.b/c/d `src/NodeCanvas.tsx` — composant qui rend `<ReactFlow>` avec mini-palette temporaire (4 boutons Trigger/Action/If/Loop — la palette riche est F8.3), state local `nodes`/`edges`, `onConnect` natif pour créer une edge en drag entre handles, `deleteKeyCode={["Backspace","Delete"]}` pour la suppression, `<Controls/>` + `<MiniMap pannable zoomable/>` + `<Background variant="dots"/>` ; chaque mutation propage la sérialisation au callback `onChange`. F8.2.e `src/graph.ts` — `serializeGraph`/`deserializeGraph` qui matchent verbatim le format `serde` du backend (`#[serde(tag = "kind", rename_all = "snake_case")]` + `#[serde(flatten)]` sur kind = on émet `{id, name, kind: "trigger", trigger_type, params}` aplati ; Edge → `{from, to, from_port?}` ; `WorkflowGraph` complet). Helpers `buildCanvasNode`, `generateNodeId` (`<kind>-<n>` avec collision check), `DEFAULT_POSITION`. Test-setup étendu : stubs `ResizeObserver` + `DOMMatrixReadOnly` (jsdom ne les expose pas, React Flow les requiert). 12 nouveaux tests JS (7 `graph.test.ts` : kind tagged, trigger_type, action_type, from_port, round-trip, generateNodeId ; 5 `NodeCanvas.test.tsx` : empty + serialized, ajout palette, ids uniques, ReactFlow + Controls + MiniMap montés, rehydratation initialNodes). Total frontend : **18 tests verts** ; `pnpm build` 328 kB JS gzip 106 kB. | — |
| 2026-05-30 | F8.3 | Palette de nodes (triggers / actions / logique) : F8.3.a `src/catalog.ts` — type `CatalogItem { id, kind, subtype?, label, category, description? }` + catalogue statique embed (12 items : 3 triggers webhook/schedule/polling, 5 actions http_request/slack.send/gmail.send/notion.create_page/github.create_issue, 4 logique if/switch/loop/while), helpers `searchCatalog` (case-insensitive sur label + subtype + description) et `groupByCategory` (ordre canonique Triggers → Actions → Logique). F8.3.b `src/Palette.tsx` — sidebar 288 px avec champ recherche (`useDeferredValue`), sections par catégorie, items `draggable` qui posent un MIME custom `application/x-autome-node` dans `dataTransfer` ; refactor `NodeCanvas.tsx` (wrap dans `ReactFlowProvider` pour exposer `useReactFlow`) : la mini-palette de F8.2 est retirée au profit du drop natif (`onDragOver` preventDefault + `onDrop` qui lit le MIME, parse, calcule position via `screenToFlowPosition`, génère un `CanvasNode` à la position du curseur). F8.3.c `fetchCatalog()` — interroge `GET /catalog/nodes` du backend ; **fallback silencieux** sur `STATIC_CATALOG` si l'endpoint n'existe pas (404), si le transport échoue, ou si la réponse est mal formée (parsing strict via `coerceCatalogItem`). 19 nouveaux tests JS (13 `catalog.test.ts` : invariants statiques + recherche + groupBy + fetch happy path / 404 / transport error / malformed / mixed valid+invalid entries ; 5 `Palette.test.tsx` : rendu catégories, filtre, no-result, loading, MIME draggable ; +1 dans `NodeCanvas.test.tsx` : drop sans MIME ignoré). Total frontend : **37 tests verts** ; `pnpm build` 333 kB JS gzip 108 kB. | — |
| 2026-05-31 | F8.4 | Panneau de configuration d'un node : F8.4.a `src/paramSchema.ts` — type `ParamSchema { name, label, type: "string"\|"text"\|"number"\|"boolean"\|"enum", required?, default?, options?, pattern?, min?, max?, placeholder?, help? }` + `NodeSchema { kind, subtype?, params, credentialProvider? }` + registre statique `NODE_SCHEMAS` (12 entrées couvrant les 12 items du catalogue F8.3) ; `findNodeSchema(kind, subtype)`. `src/NodePanel.tsx` — sidebar droite 320 px qui rend dynamiquement les champs depuis le schéma (`input[type=text\|number]`, `textarea`, `select`, `checkbox`) — aucun composant ad-hoc par type. F8.4.b `defaultParams(schema)` + `validateParams(schema, values)` — required, type-check, regex pattern (string/text), bornes min/max (number), options (enum). Defaults appliqués à l'**ajout par drop** (NodeCanvas appelle `defaultParams(findNodeSchema(item))` avant `buildCanvasNode`, qui accepte désormais `params?: unknown`). Erreurs inline (`role="alert"`, `aria-invalid`) + statut global du panneau (« Configuration valide. » / « N erreurs à corriger. »). Édition **live** : chaque modification déclenche `onChange(nodeId, params)` immédiatement, pas de bouton Save. F8.4.c `extractReferences(value)` — regex `/\{\{\s*\$node\(['"]id['"]\)(\.path)?\s*\}\}/g` qui extrait les références (raw, nodeId, path?) ; chips d'affichage sous chaque champ string/text détectant une référence. F8.4.d `src/credentials.ts` — `fetchCredentials(provider)` interroge `GET /credentials?provider=<x>` (F7.3) avec **fallback silencieux** `[]` si 404, transport KO, ou payload mal formé ; `NodePanel` affiche une dropdown alimentée si le schéma déclare `credentialProvider` (slack/gmail/notion/github) — la valeur sélectionnée est stockée dans `params.credential_id`. Wiring : `NodeCanvas` expose `onSelectionChange(node\|null)` (driven par `<ReactFlow onSelectionChange>`) + `onReady(controller)` (controller imperatif `patchNodeParams(id, params)`) ; `App.tsx` orchestre `selectedNode` state + appelle `controller.patchNodeParams` quand le panel pousse un `onChange`. 49 nouveaux tests JS (26 `paramSchema.test.ts` : registre + findNodeSchema + defaults + validation required/pattern/min/max/enum + 6 cas `extractReferences` + 3 `coerceNumberInput` ; 6 `credentials.test.ts` : URL + encoding + 404/transport/malformed/entries filtrées ; 17 `NodePanel.test.tsx` : empty/unknown, génération formulaire par type, defaults pré-remplis, validation live, refs détectées, dropdown credentials peuplée/vide/non rendue/propagée, bouton fermer ; +2 `NodeCanvas.test.tsx` : defaults appliqués au drop + controller patch). Total frontend : **88 tests verts** ; `pnpm build` 345 kB JS gzip 111 kB. | — |
| 2026-05-31 | F8.5 | Visualisation des exécutions : F8.5.a `src/RunsList.tsx` — sidebar gauche 320 px qui consomme `GET /workflows/{id}/runs?offset=&limit=` (F7.2.d) ; chaque run rendu avec badge statut (queued/running/succeeded/failed), `trigger_type`, durée formatée (`formatDuration` : `< 1s` → ms, `< 60s` → s.x, sinon `m min ss s`, « en cours » si pas de `finished_at`, `—` si pas de `started_at`) et heure de création ; pagination offset/limit qui s'affiche seulement si `total > pageSize` ; refresh manuel via bouton ↻. F8.5.b `src/runs.ts` — `nodeExecutionStatus(checkpoint, nodeId)` : `succeeded` si node ∈ `outputs`, `running` si `node == next_node && status == Running`, `failed` si `node == next_node && status == Failed`, `pending` sinon ; `decorateNodesWithExecStatus(nodes, statuses)` applique `style.borderColor` (+ `data.execStatus`) à chaque node React Flow via palette dédiée (`execStatusColor`). `NodeCanvas` accepte un prop optionnel `executionStatuses?: Record<id, NodeExecStatus>` qui décore les nodes en place (pas de fork des nodes éditables). F8.5.c `src/RunExecutionPanel.tsx` — sidebar droite read-only qui affiche, pour un (run, node) sélectionnés : badge statut d'exécution du node + section **Input** (« Payload du trigger » pour les nodes racines via `inputForNode` qui détecte l'absence de prédécesseurs, sinon outputs des prédécesseurs immédiats indexés par `node_id`, ou « Aucune sortie de prédécesseur » si aucun n'a encore tourné) + section **Output** (`outputs[node_id]` rendu via `JSON.stringify(_, null, 2)` ou message contextuel selon le statut : « Node non exécuté » / « Exécution en cours… » / « Aucune sortie produite ») + bloc **Erreur** quand `status == failed && checkpoint.error != null` ; bouton fermer. F8.5.d `src/useRunPolling.ts` — hook qui re-fetch `GET /runs/{id}` (`fetchRun`) toutes les `intervalMs` (défaut 2 000 ms), s'arrête automatiquement dès qu'un statut **terminal** (`Succeeded`/`Failed`) est observé via `isTerminalStatus`, retente sur erreur (le 404 transitoire pendant la race avec le dispatcher est non-bloquant), redémarre proprement quand `runId` change, et nettoie son timer au unmount. **Wiring App** : nouveau toggle `View = "edit" | "runs"` en `role="tablist"` ; en mode `runs`, la `Palette` est remplacée par `RunsList` et le `NodePanel` par `RunExecutionPanel` ; le `NodeCanvas` reste monté entre les deux modes (sa state survit au switch — le workflow édité reste à l'écran lors de l'inspection d'un run). 51 nouveaux tests JS (29 `runs.test.ts` : `isTerminalStatus`, `formatDuration` (6 cas incl. clamp), `formatTime`, `nodeExecutionStatus` (6 cas dont priorité succeeded > running), `inputForNode` (3), `execStatusColor`, `decorateNodesWithExecStatus`, `fetchRunsForWorkflow` (5 cas dont URL encoding + filtrage items mal formés), `fetchRun` (5) ; 7 `useRunPolling.test.tsx` avec `vi.useFakeTimers` + `advanceTimersByTimeAsync` (null no-op, fetch initial, polling périodique, arrêt sur terminal, retry après erreur, restart sur change runId, cleanup unmount) ; 9 `RunsList.test.tsx` (mount fetch, page vide, badges par statut, sélection, état sélectionné, erreur, refresh, pagination conditionnelle, navigation page) ; 10 `RunExecutionPanel.test.tsx` (vide checkpoint, vide node, statut dérivé, output JSON, output « pas exécuté », input trigger, input prédécesseurs, input vide, bloc erreur, close) ; +1 canvas (executionStatuses applique borderColor) ; +1 `App.test.tsx` (tab Exécutions monte la RunsList et fetche les runs)). Total frontend : **148 tests verts** ; `pnpm typecheck` + `pnpm lint` + `pnpm build` (358 kB JS gzip 114 kB) verts. Hors scope (à planifier) : WS/SSE backend (on reste en polling tant que le serveur n'expose pas d'événements), annulation/retry d'un run depuis l'UI, persistance/historique de logs structurés par node. | — |
| 2026-05-31 | F8.7 | Intégration API backend — issue #93 : F8.7.c `src/authFetch.ts` — `createAuthFetch({ getTokens, onTokens, onUnauthenticated, refreshImpl?, fetchImpl? })` enveloppe un `fetch` pour (1) injecter `Authorization: Bearer <access>` (sans écraser un header explicite du caller), (2) sur **401** rafraîchir le token **une seule fois** puis rejouer la requête, (3) `onUnauthenticated` + 401 d'origine si le refresh échoue ou si aucun token. Refresh **single-flight** (un seul `/auth/refresh` pour N 401 concurrents). F8.7.b `src/useApiQuery.ts` — hook `{ data, loading, error, refetch }` + cache mémoire partagé (clé → données + ts), `staleTimeMs` (sert le cache frais sans refetch), `enabled`, invalidation (`invalidateQuery`/`setQueryData`/`getQueryData`), compteur de génération pour ignorer les résultats périmés (clé changée / démontage). F8.7.a génération **depuis OpenAPI** : `crates/api/examples/dump_openapi.rs` (`cargo run -p autome-api --example dump_openapi`) sérialise `ApiDoc::openapi()` sans DB → snapshot `frontend/openapi.json` ; devDep `openapi-typescript` + script `pnpm gen:api` → `src/apiSchema.ts` (généré, hors lint) ; façade typée `src/apiClient.ts` (`createApiClient(fetchImpl)`) couvrant workflows (CRUD), runs (list/create/get, délégués aux helpers tolérants de `runs.ts`) et credentials (providers/list/authorize/callback/delete), types des enveloppes tirés du schéma généré ; `API_PATHS` (templates `{param}` verbatim) + test **anti-drift** (chemins client ⊂ `openapi.json`). **Wiring** : `useSession` expose un `authFetch` stable (réf. synchrone vers la session, `onTokens`=persist, `onUnauthenticated`=logout) ; `App` construit `createApiClient(authFetch)` (mémoïsé) et le branche sur `RunsList` (`fetchImpl`) + `useRunPolling` (`fetchImpl: api.getRun`) — les lectures de runs portent désormais le Bearer et bénéficient du refresh sur 401. Deps : `@types/node` (devDep, lecture du snapshot dans le test anti-drift) + `node` dans `types` de `tsconfig.app`. 34 nouveaux tests JS (7 `authFetch.test.ts`, 8 `useApiQuery.test.tsx`, 16 `apiClient.test.ts`, +3 `useSession.test.tsx` authFetch). Total frontend : **258 tests verts** ; `pnpm typecheck` + `pnpm lint` + `pnpm build` (376 kB JS gzip 120 kB) verts ; `cargo fmt --check` + `clippy` (example) verts. **Domaine 8 (Frontend & Éditeur visuel) terminé — 7/7.** Hors scope : génération auto du client en CI (on commit le snapshot + types), persistance/list des workflows côté UI (le brouillon `wf-draft` reste local — client CRUD prêt pour quand l'UI de gestion de workflows arrivera). | — |
| 2026-05-31 | F8.6 | Auth UI (login, session, connecteurs OAuth) — PR #92 : F8.6.a `src/auth.ts` (`login`/`signup`/`refresh`/`whoami` contre `/auth/*`, validation pure email + longueur mot de passe, décodage JWT local non vérifié pour lire `sub`/`exp`) + `src/LoginPage.tsx` (formulaire login/signup, erreurs inline). F8.6.b `src/session.ts` (`SessionStore` injectable, `webSessionStore`/`memorySessionStore`, `sessionFromTokens`, `msUntilRefresh`) + `src/useSession.ts` (hook source de vérité : charge la session au montage, expose `login`/`signup`/`logout`, refresh proactif `skewMs` avant l'expiration, déconnexion propre sur échec). F8.6.c `src/ConnectorsPanel.tsx` + `src/connectors.ts` (liste providers, lancement autorisation OAuth, résumé des credentials). Garde d'auth dans `App.tsx` (LoginPage tant que pas de session). Tests Vitest + RTL verts. | — |
| 2026-05-31 | F9.1 | Tests unitaires moteur de workflows — issue #95 : ouverture du **domaine 9 (Tests TDD)**. (1) Suite d'intégration **black-box** `crates/engine/tests/workflow_engine.rs` (9 tests) qui pilote le moteur via sa **seule API publique** (`Dag::from_workflow`, `run_sequential`/`run_parallel`, `RunContext`/`RunReport`) sur des workflows composés réalistes — pipeline linéaire + mapping `{{ }}` (params résolus côté runner, sortie de prédécesseur lue), routage conditionnel `If` (port non pris → `Skipped`), boucle `Loop` dont l'agrégat est consommé par un node aval, diamant fan-out/fan-in en parallèle (jointure voit les deux branches), retry transitoire→succès, `on_error: fallback` routé vers handler `error` (séquentiel **et** parallèle), échec non géré → fail-fast aval `Skipped` (les deux moteurs), **propriété** d'équivalence séquentiel↔parallèle sur graphe large (30 branches fan-out→fan-in : même statut par node + global), rejet d'un workflow **cyclique** à la construction. (2) Renfort unitaire des modules fins : `condition.rs` +4 tests (`json_eq` normalisation num/collections, index tableau hors-borne + descente dans scalaire → `Err` sans panique, opérateurs `>=`/`!=`/négatifs/flottants, comparaison ordonnée hétérogène → `Err`, truthiness `[]`/`{}`/`0.0`) ; `mapping.rs` +3 tests (`value_to_string` bool/null/number/array inline, pass-through des scalaires non-chaîne, `sole_expression` tolère les espaces mais bascule en templating sur 2 tags). Engine : **51 inline (+7) + 9 intégration = 60 tests**. `cargo test -p autome-engine` + `cargo fmt --check` + `cargo clippy -p autome-engine --all-targets -D warnings` verts. | — |
