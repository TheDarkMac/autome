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
| 2  | Moteur de workflows      | 7        | 1         | En cours      |
| 3  | Triggers & Actions       | 6        | 0         | À faire       |
| 4  | Connecteurs API          | 5        | 0         | À faire       |
| 5  | Orchestrateur & Scaling  | 6        | 0         | À faire       |
| 6  | Sécurité & Auth          | 6        | 0         | À faire       |
| 7  | API publique (REST/gRPC) | 4        | 0         | À faire       |
| 8  | Frontend & Éditeur visuel| 7        | 0         | À faire       |
| 9  | Tests (TDD)              | 5        | 0         | À faire       |
| 10 | DevOps & Déploiement     | 6        | 0         | À faire       |
| 11 | Documentation            | 5        | 0         | À faire       |
| 12 | Exemples de workflows    | 3        | 0         | À faire       |

**Progression totale : 7 / 66 features** (≈ 150 sous-tâches sur les features détaillées)

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
- [ ] **F2.2** — Exécution séquentielle des nodes
  - [ ] F2.2.a — Tri topologique
  - [ ] F2.2.b — Boucle d'exécution + contexte de run
  - [ ] F2.2.c — Collecte des résultats par node
- [ ] **F2.3** — Branches conditionnelles (if / switch)
  - [ ] F2.3.a — Node `If` (true/false)
  - [ ] F2.3.b — Node `Switch` (n branches)
  - [ ] F2.3.c — Évaluation d'expression de condition
- [ ] **F2.4** — Boucles (iterate sur collections, while)
  - [ ] F2.4.a — Node `Loop` (iterate sur collection)
  - [ ] F2.4.b — Node `While` (condition + garde anti-boucle infinie)
  - [ ] F2.4.c — Agrégation des sorties d'itérations
- [ ] **F2.5** — Exécution parallèle (fan-out / fan-in)
  - [ ] F2.5.a — Fan-out (lancement concurrent des branches)
  - [ ] F2.5.b — Fan-in (synchronisation / jointure des résultats)
  - [ ] F2.5.c — Limite de concurrence configurable
- [ ] **F2.6** — Passage & mapping de données entre nodes (expressions / templating)
  - [ ] F2.6.a — Moteur d'expressions (accès aux sorties précédentes)
  - [ ] F2.6.b — Templating de chaînes (`{{ }}`)
  - [ ] F2.6.c — Mapping/transformation des champs d'entrée
- [ ] **F2.7** — Gestion d'erreurs : retry, fallback, timeout par node
  - [ ] F2.7.a — Politique de retry (nb essais, backoff)
  - [ ] F2.7.b — Timeout par node
  - [ ] F2.7.c — Branche/handler d'erreur (fallback)
  - [ ] F2.7.d — Propagation & statut final du run

## 3. Triggers & Actions

- [ ] **F3.1** — Trait/abstraction `Trigger` extensible
- [ ] **F3.2** — Trigger Webhook (HTTP entrant)
- [ ] **F3.3** — Trigger Scheduler (cron / intervalle)
- [ ] **F3.4** — Trigger Polling (vérification périodique d'une source)
- [ ] **F3.5** — Trait/abstraction `Action` extensible
- [ ] **F3.6** — Action HTTP générique (requête REST configurable)

## 4. Connecteurs API

- [ ] **F4.1** — Connecteur Slack (envoi message, lecture canal)
  - [ ] F4.1.a — Auth Slack (OAuth / bot token)
  - [ ] F4.1.b — Action `send_message`
  - [ ] F4.1.c — Action `read_channel` / lecture historique
  - [ ] F4.1.d — Mapping erreurs API → erreurs moteur
- [ ] **F4.2** — Connecteur Gmail (envoi / lecture mail)
  - [ ] F4.2.a — Auth Gmail (OAuth2 Google)
  - [ ] F4.2.b — Action `send_email`
  - [ ] F4.2.c — Action `list/read messages`
  - [ ] F4.2.d — Gestion pièces jointes
- [ ] **F4.3** — Connecteur Notion (création / lecture page)
  - [ ] F4.3.a — Auth Notion (token intégration)
  - [ ] F4.3.b — Action `create_page`
  - [ ] F4.3.c — Action `read_page` / query database
  - [ ] F4.3.d — Conversion blocs Notion ↔ données workflow
- [ ] **F4.4** — Connecteur GitHub (issues, PR, webhooks)
  - [ ] F4.4.a — Auth GitHub (PAT / GitHub App)
  - [ ] F4.4.b — Actions issues (créer, commenter, fermer)
  - [ ] F4.4.c — Actions PR (lister, créer, merger)
  - [ ] F4.4.d — Réception webhooks GitHub (vérif signature)
- [ ] **F4.5** — SDK interne pour créer un connecteur (doc + template)
  - [ ] F4.5.a — Trait `Connector` + cycle de vie auth
  - [ ] F4.5.b — Helpers HTTP (rate-limit, retry, pagination)
  - [ ] F4.5.c — Schéma de déclaration des actions/params
  - [ ] F4.5.d — Template/exemple de connecteur + doc

## 5. Orchestrateur & Scaling

- [ ] **F5.1** — File de messages (Kafka ou RabbitMQ) — choix + intégration
  - [ ] F5.1.a — Décision Kafka vs RabbitMQ (critères documentés)
  - [ ] F5.1.b — Abstraction `Queue` (publish/consume)
  - [ ] F5.1.c — Implémentation concrète + config connexion
  - [ ] F5.1.d — Dead-letter queue
- [ ] **F5.2** — Worker pool pour exécution distribuée des runs
  - [ ] F5.2.a — Consumer worker (pull depuis la file)
  - [ ] F5.2.b — Pool de tâches concurrentes + arrêt propre
  - [ ] F5.2.c — Ack/nack & idempotence
- [ ] **F5.3** — Planificateur de jobs (dispatch triggers → file)
  - [ ] F5.3.a — Scheduler cron/intervalle
  - [ ] F5.3.b — Dispatch trigger → message de run
  - [ ] F5.3.c — Anti-double-déclenchement (lock distribué)
- [ ] **F5.4** — Persistance de l'état d'exécution (reprise après crash)
  - [ ] F5.4.a — Checkpoint de l'état du run en base
  - [ ] F5.4.b — Reprise d'un run interrompu
  - [ ] F5.4.c — Nettoyage des runs orphelins
- [ ] **F5.5** — Backpressure & limites de concurrence
- [ ] **F5.6** — Test de charge (objectif : milliers de workflows simultanés)

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
