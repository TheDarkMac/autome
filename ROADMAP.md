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
| 6  | Sécurité & Auth          | 6        | 0         | À faire       |
| 7  | API publique (REST/gRPC) | 4        | 0         | À faire       |
| 8  | Frontend & Éditeur visuel| 7        | 0         | À faire       |
| 9  | Tests (TDD)              | 5        | 0         | À faire       |
| 10 | DevOps & Déploiement     | 6        | 0         | À faire       |
| 11 | Documentation            | 5        | 0         | À faire       |
| 12 | Exemples de workflows    | 3        | 0         | À faire       |

**Progression totale : 30 / 66 features** (≈ 150 sous-tâches sur les features détaillées)

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

- [ ] **F6.1** — Authentification utilisateur (JWT)
  - [ ] F6.1.a — Signup / hash mot de passe (argon2)
  - [ ] F6.1.b — Login → émission JWT (access + refresh)
  - [ ] F6.1.c — Middleware de vérification du token
  - [ ] F6.1.d — Révocation / rotation des tokens
- [ ] **F6.2** — OAuth2 (login + autorisation connecteurs tiers)
  - [ ] F6.2.a — Flow Authorization Code (login)
  - [ ] F6.2.b — Stockage & refresh des tokens connecteurs
  - [ ] F6.2.c — Gestion multi-provider
- [ ] **F6.3** — Chiffrement des credentials/secrets au repos
  - [ ] F6.3.a — Choix de la stratégie (AES-GCM + KMS / env key)
  - [ ] F6.3.b — Chiffrement/déchiffrement transparent en base
  - [ ] F6.3.c — Rotation de clé
- [ ] **F6.4** — TLS sur tous les services
- [ ] **F6.5** — RBAC (rôles & permissions)
  - [ ] F6.5.a — Modèle rôles/permissions
  - [ ] F6.5.b — Enforcement au niveau API (guards)
- [ ] **F6.6** — Audit log des actions sensibles

## 7. API publique (REST / gRPC)

- [ ] **F7.1** — CRUD workflows (créer, lister, modifier, supprimer)
- [ ] **F7.2** — Déclenchement manuel & suivi d'un run
- [ ] **F7.3** — Endpoints credentials / connecteurs
- [ ] **F7.4** — Spec OpenAPI (ou proto gRPC) générée

## 8. Frontend & Éditeur visuel

- [ ] **F8.1** — Setup React + TypeScript + TailwindCSS
- [ ] **F8.2** — Canvas drag-and-drop (nodes + connexions)
  - [ ] F8.2.a — Choix de la lib (React Flow / xyflow)
  - [ ] F8.2.b — Rendu des nodes + ajout par drag depuis la palette
  - [ ] F8.2.c — Création/suppression de connexions
  - [ ] F8.2.d — Zoom / pan / mini-map
  - [ ] F8.2.e — Sérialisation du graphe ↔ modèle backend
- [ ] **F8.3** — Palette de nodes (triggers, actions, logique)
  - [ ] F8.3.a — Catalogue des nodes (catégories + recherche)
  - [ ] F8.3.b — Drag depuis la palette vers le canvas
  - [ ] F8.3.c — Chargement dynamique depuis l'API (nodes dispo)
- [ ] **F8.4** — Panneau de configuration d'un node
  - [ ] F8.4.a — Formulaire généré depuis le schéma de params
  - [ ] F8.4.b — Validation des champs + valeurs par défaut
  - [ ] F8.4.c — Éditeur d'expressions / mapping de données
  - [ ] F8.4.d — Sélection de credentials/connecteur
- [ ] **F8.5** — Visualisation des exécutions (statut, logs par node)
  - [ ] F8.5.a — Liste des runs (statut, durée, déclencheur)
  - [ ] F8.5.b — Vue détail : statut par node sur le canvas
  - [ ] F8.5.c — Logs & données d'entrée/sortie par node
  - [ ] F8.5.d — Mise à jour temps réel (WS/SSE ou polling)
- [ ] **F8.6** — Auth UI (login, gestion connecteurs)
  - [ ] F8.6.a — Pages login / signup
  - [ ] F8.6.b — Gestion de session (stockage token, refresh)
  - [ ] F8.6.c — Écran connexion/autorisation OAuth des connecteurs
- [ ] **F8.7** — Intégration API backend
  - [ ] F8.7.a — Client API typé (généré depuis OpenAPI)
  - [ ] F8.7.b — Couche data-fetching (cache, états loading/erreur)
  - [ ] F8.7.c — Intercepteur auth (token + 401 → refresh)

## 9. Tests (TDD)

- [ ] **F9.1** — Tests unitaires moteur de workflows
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
