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
| `QUEUE_URL` | URL de la file (RabbitMQ). Le chemin est le vhost : `…:5672/%2f` pour le vhost par défaut `/` | *(vide)* | **obligatoire** (avec `AUTOME_QUEUE=rabbitmq`) |
| `AUTOME_PERSISTENCE` | Stores de l'API et du worker : `memory` ou `postgres` | `memory` | **`postgres` obligatoire** |
| `AUTOME_QUEUE` | File des runs : `memory` ou `rabbitmq` | `memory` | **`rabbitmq` obligatoire** |
| `AUTOME_QUEUE_CONNECT_TIMEOUT_SECS` | Délai pendant lequel l'API et le worker retentent la connexion au broker au démarrage (erreurs réseau seulement) | `60` | `60` |
| `AUTOME_MIGRATE` | L'API applique les migrations au démarrage (`true`/`1`/`yes`, mode `postgres`) | *(non)* | recommandé |
| `JWT_SECRET` | Clé de signature des JWT | `dev-insecure-jwt-secret` | **obligatoire** (non-défaut) |
| `ENCRYPTION_KEY` | Clé de chiffrement des credentials au repos — **identique pour l'API et le worker** | `dev-insecure-encryption-key` | **obligatoire** (non-défaut) |
| `AUTOME_TLS_CERT` | Chemin du certificat PEM (active HTTPS) | *(non défini)* | optionnel |
| `AUTOME_TLS_KEY` | Chemin de la clé privée PEM | *(non défini)* | optionnel |
| `AUTOME_EGRESS_ALLOWLIST` | Hôtes internes autorisés en sortie HTTP (anti-SSRF), séparés par virgule | *(vide : public uniquement)* | selon besoin |
| `AUTOME_LOG_FORMAT` | Format des logs (API + worker) : `json` ou `text` | `text` | `json` (défaut en `prod`) |
| `RUST_LOG` | Niveau des logs, syntaxe `EnvFilter` (ex. `info,autome_worker=debug`) | `info` | `info` |
| `AUTOME_WORKER_CONCURRENCY` | Worker : tâches consommatrices par process | `4` | selon charge |
| `AUTOME_RUN_TIMEOUT_SECS` | Worker : au-delà, un run encore `running` est clos en `failed` (min. 60) | `3600` | > durée du plus long run |
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

### Perte du broker (RabbitMQ)

Si RabbitMQ redémarre ou devient injoignable, l'API et le worker n'ont pas à
être redémarrés : chaque appel suivant rouvre la connexion (une tentative,
10 s au plus). Pendant la panne, le déclenchement d'un run répond en erreur et
le worker espace ses tentatives (pause doublée à chaque échec, 5 s au plus) ;
le retour est journalisé (`connexion RabbitMQ rétablie`). Un run en cours de
livraison au moment de la coupure est remis en file par le broker et relivré —
sans double exécution grâce au claim en base (section suivante). Les
publications sont **confirmées** par le broker : un message refusé fait échouer
le déclenchement au lieu d'être perdu silencieusement.

### Exécution des runs : au plus une fois

RabbitMQ livre chaque message **au moins une fois** : après un crash ou une
perte de connexion, un run peut être relivré, à n'importe quelle réplique du
worker. Pour que ses effets de bord (message Slack, e-mail, issue GitHub…) ne
se produisent pas deux fois, le worker **réclame** le run en base avant de
l'exécuter (`queued → running`, atomique) : une seule réplique gagne, les
livraisons dupliquées d'un run déjà pris ou terminé sont acquittées sans
ré-exécution.

Corollaire : un run interrompu en cours d'exécution (worker tué, pod évincé)
**n'est pas rejoué**. Toutes les 60 s, chaque worker clôt en `failed` les runs
encore `running` au-delà de `AUTOME_RUN_TIMEOUT_SECS` (défaut 1 h), avec une
erreur explicite ; relancer le workflow si besoin. Réglez ce délai au-dessus de
la durée du plus long run légitime : sinon un run encore actif serait affiché
en échec jusqu'à ce que son état final le corrige.

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

L'API et le worker y partagent **PostgreSQL** (`AUTOME_PERSISTENCE=postgres`)
et **RabbitMQ** (`AUTOME_QUEUE=rabbitmq`) ; l'API applique les migrations au
démarrage (`AUTOME_MIGRATE=true`). Les données survivent à un
`docker compose restart` (volume `pgdata`).

| Service | URL | Identifiants |
|---|---|---|
| API | http://localhost:8080 | — (`/livez`, `/readyz`, `/metrics`) |
| Frontend | http://localhost:8081 | — |
| Grafana | http://localhost:3000 | `admin` / `admin` |
| Prometheus | http://localhost:9090 | — |
| RabbitMQ (UI) | http://localhost:15672 | `guest` / `guest` |
| PostgreSQL | `localhost:5432` | `postgres` / `postgres` |

Vérification rapide :

```bash
curl http://localhost:8080/readyz     # -> {"status":"ready","checks":{"postgres":"ok","rabbitmq":"ok"}}
curl http://localhost:8080/metrics    # -> métriques Prometheus
docker compose ps                     # api et worker « healthy »
```

Vérification **de bout en bout** — compte jetable, workflow, run exécuté par
le worker jusqu'à `succeeded` (échoue si l'API et le worker ne partagent pas
la file) :

```bash
scripts/compose-smoke.sh
```

Compte super-admin de dev (optionnel) : appliquer le seed **après** le premier
démarrage de l'API (migrations appliquées), puis se connecter avec
`dev@autome.local` / `autome-dev-password` :

```bash
docker compose exec -T db psql -U postgres -d autome < migrations/seeds/dev_seed.sql
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
`0011_connector_tokens.sql`), embarqué dans le binaire de l'API. Pour
l'appliquer sur votre base, démarrez l'API une fois avec le runner de
migrations — idempotent, journalisé dans `schema_migrations`, sérialisé entre
répliques par un verrou applicatif :

```bash
AUTOME_PERSISTENCE=postgres AUTOME_MIGRATE=true cargo run -p autome-api
```

> N'appliquez pas les fichiers à la main (`psql -f`) sur une base que l'API
> gérera ensuite : le journal `schema_migrations` ne serait pas alimenté et un
> démarrage avec `AUTOME_MIGRATE=true` tenterait de tout réappliquer.

Pour seulement **valider** les migrations sur un PostgreSQL jetable
(conteneur Docker dédié, sans effet sur une base existante) :

```bash
scripts/check-migrations.sh
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
curl http://localhost:8080/readyz     # -> {"status":"ready",…}
kubectl -n autome get pods            # api et worker READY 1/1
```

### C.5 Sondes : vivant et prêt (#192)

« Vivant » et « prêt » sont deux questions distinctes, aux conséquences
opposées :

| Sonde | API (`:8080`) | Worker (`:9091`) | Échec ⇒ |
|---|---|---|---|
| **Liveness** `GET /livez` | le process répond | chaque consommateur tourne, ou traite un run depuis moins de `AUTOME_RUN_TIMEOUT_SECS` + 1 min | **redémarrage** du conteneur |
| **Readiness** `GET /readyz` | PostgreSQL (`SELECT 1`) et RabbitMQ (aller-retour) joignables | idem | retiré du Service / déploiement suspendu, **sans** redémarrage |

- La liveness ne vérifie **aucune** dépendance : une panne de base ferait
  sinon redémarrer en boucle tous les pods sans rien réparer. Une base qui ne
  répond plus rend les pods « pas prêts » ; ils redeviennent prêts seuls à son
  retour.
- Chaque vérification de `/readyz` est bornée à 2 s et elles tournent en
  parallèle (sonde k8s : `timeoutSeconds: 5`). Réponse :
  `{"status":"ready"|"unavailable","checks":{"postgres":"ok","rabbitmq":"error"}}`
  (200 ou 503) ; le détail de l'erreur n'est que dans les logs.
- Un run long ne fait pas tuer le worker : un consommateur occupé reste
  « vivant » jusqu'au délai d'un run. Au-delà (moteur bloqué), ou si un
  consommateur au repos ne tourne plus depuis 60 s, `/livez` répond 503 ;
  après redémarrage, le balayage des orphelins clôt les runs interrompus.
- Une **startup probe** (~90 s) couvre les migrations et l'attente du broker
  au démarrage avant que la liveness ne s'applique.
- Worker : `AUTOME_WORKER_PROBE_ADDR` (défaut `127.0.0.1:9091`, `0.0.0.0:9091`
  dans l'image ; vide = sondes désactivées).
- `/health` reste un alias de `/livez` (compatibilité).
- **Compose** : healthchecks sur `/readyz` (API) et `/livez` (worker) ; le
  worker attend que l'API soit *prête* (migrations appliquées) pour démarrer.

**Arrêt gracieux de l'API (#194).** Sur `SIGTERM` (déploiement progressif,
réduction d'échelle, éviction) ou `SIGINT` :

1. `/readyz` répond aussitôt **503 `{"status":"draining"}`** ; `/livez` reste
   200 (le pod ne doit pas être tué en plein arrêt) ;
2. pendant `AUTOME_SHUTDOWN_DELAY_SECS` (défaut **5 s**), l'API **continue de
   servir** normalement : k8s retire le pod des endpoints en parallèle du
   signal, et l'ingress peut encore lui envoyer des requêtes quelques secondes ;
3. elle cesse ensuite d'accepter des connexions et laisse finir les requêtes
   en cours, au plus `AUTOME_SHUTDOWN_TIMEOUT_SECS` (défaut **20 s**) —
   au-delà, elles sont interrompues (avertissement dans les logs) ;
4. sortie avec le code 0 (`autome-api arrêtée`).

HTTP et HTTPS se comportent de même. La période de grâce de l'orchestrateur
doit dépasser `délai + drain` : `terminationGracePeriodSeconds: 35` dans
`k8s/api.yaml`, `stop_grace_period: 35s` dans compose. En local, `Ctrl-C`
suit la même séquence (`AUTOME_SHUTDOWN_DELAY_SECS=0` pour s'arrêter sans
attendre).

### C.6 Monitoring

Le namespace inclut **Prometheus** (scrape de l'API) et **Grafana** (datasource
+ dashboard *Autome — Overview* provisionnés) :

```bash
kubectl -n autome port-forward svc/grafana 3000:3000     # http://localhost:3000 (admin/admin)
kubectl -n autome port-forward svc/prometheus 9090:9090  # http://localhost:9090
```

**Logs** : en `prod`, l'API et le worker écrivent **une ligne JSON par
événement** sur la sortie standard, prête pour un collecteur (Loki, Fluent Bit,
CloudWatch…). Champs utiles :

- `level`, `message`, `target`, `timestamp`, puis les champs de l'événement à
  plat (`status`, `duration_ms`, `error`, `verdict`…) ;
- `span.request_id` (API) : identifiant de la requête, renvoyé au client dans
  l'en-tête `x-request-id` — un utilisateur peut le citer pour retrouver ses
  logs. Un `x-request-id` fourni par le proxy est repris s'il est sûr
  (alphanumérique et `-_.:`, 128 caractères max), sinon un id est généré ;
- `span.run_id` / `span.workflow_id` (worker) : tous les logs d'un run, y
  compris ceux des connecteurs.

Le chemin est journalisé sans la query string. Pour diagnostiquer, monter le
niveau d'un module sans redéployer l'image : `RUST_LOG=info,autome_worker=debug`.

---

## Authentification (comptes & jetons)

L'API émet ses propres JWT (HS256, signés avec `JWT_SECRET`). Les endpoints de
comptes sont publics ; tout le reste exige `Authorization: Bearer <access_token>`.

| Méthode | Path | Body | Réponse |
|---|---|---|---|
| POST | `/auth/signup` | `{"email","password"}` | 201 + paire de jetons · 400 saisie invalide · 409 email déjà pris |
| POST | `/auth/login` | `{"email","password"}` | 200 + paire de jetons · 401 identifiants invalides · 429 trop d'échecs (`Retry-After`) |
| POST | `/auth/refresh` | `{"refresh_token"}` | 200 + nouvelle paire · 401 refresh invalide, expiré ou déjà utilisé |
| POST | `/auth/logout` | `{"refresh_token"}` (+ `Authorization: Bearer <access_token>` facultatif) | 204 session fermée (aussi si déjà fermée) · 401 refresh invalide ou expiré |
| POST | `/auth/logout-all` | — (`Authorization: Bearer <access_token>` requis) | 204 toutes les sessions fermées · 401 access absent, invalide ou déjà révoqué |
| POST | `/auth/password` | `{"current_password","new_password"}` (Bearer requis) | 200 + nouvelle paire · 400 nouveau mot de passe invalide ou identique · 403 mot de passe actuel incorrect · 409 changé entre-temps · 429 trop d'essais |

- Paire de jetons : `{"access_token","refresh_token","token_type":"Bearer","expires_in":900}`.
  Access valable 15 min, refresh 7 jours et **à usage unique** (rotation) : de
  plusieurs rotations simultanées du même refresh, même sur des répliques
  différentes, une seule réussit.
- Seul l'**access token** ouvre l'API : un refresh présenté en `Bearer` est
  refusé (401).
- **Déconnexion** : `/auth/logout` révoque le refresh et, s'il est passé en
  `Bearer`, l'access token de la session (s'il appartient au même compte ;
  sinon il est ignoré). Les deux sont refusés ensuite jusqu'à leur expiration.
  Le bouton « Se déconnecter » du frontend appelle cet endpoint puis vide la
  session locale (si l'API est injoignable, les jetons expirent d'eux-mêmes).
  Chaque déconnexion est auditée (`auth.logout`). Elle ne ferme que cette
  session : les autres appareils gardent leurs jetons.
- **Déconnecter tous les appareils** : `/auth/logout-all` ferme **toutes** les
  sessions de l'utilisateur, y compris celle de la requête — après une fuite de
  jeton, un appareil perdu ou un poste partagé. Mécanisme : chaque jeton porte
  la version de session de son utilisateur (claim `sv`) ; l'appel l'incrémente
  (table `session_versions`, migration `0013`), et tout jeton d'une version
  antérieure est refusé, access comme refresh, sur toutes les répliques. Une
  nouvelle connexion ouvre une session valide. Bouton « Déconnecter tous les
  appareils » dans le frontend ; audit `auth.logout_all`. Ne jamais supprimer
  de ligne de `session_versions` : cela revaliderait les anciens jetons.
- **Changement de mot de passe** (#196) : `/auth/password` exige le mot de
  passe **actuel** (un access token volé ne suffit pas ; 5 essais par compte et
  par fenêtre de 15 min, puis 429). Le nouveau hash n'est écrit que si l'ancien
  est toujours en place (deux changements simultanés : un seul réussit, 409
  pour l'autre). Ensuite **toutes les sessions sont fermées**, sur tous les
  appareils, et la réponse fournit une nouvelle paire à l'appareil courant, qui
  reste connecté. Mot de passe actuel incorrect = **403** (pas 401, que le
  frontend interpréterait comme une session expirée). Onglet « Compte » du
  frontend ; audit `auth.password_change`. Pas encore de réinitialisation par
  email (mot de passe oublié).
- Mot de passe : 8 à 1024 caractères, haché en **Argon2id**. L'email est
  normalisé (trim + minuscules).
- Un compte créé par `signup` reçoit le rôle **editor** (CRUD workflows, runs,
  credentials) — pas d'administration.
- Anti-bruteforce : 5 tentatives sans succès par email et par fenêtre de 15 min,
  puis 429 (`Retry-After`) jusqu'à la fin de la fenêtre — même avec le bon mot
  de passe. En `AUTOME_PERSISTENCE=postgres`, le compteur est **partagé par
  toutes les répliques** de l'API et survit aux redémarrages (table
  `login_attempts`, email stocké haché) ; en `memory`, il est propre au process.
  Si le compteur est indisponible, la connexion est refusée (pas de
  contournement). Les tentatives et les révocations de jetons expirées sont
  purgées toutes les 10 min.
- **Anti-bruteforce par adresse IP** (#190) : le compteur par email ne voit ni
  la pulvérisation de mots de passe (quelques essais sur beaucoup d'emails), ni
  le bourrage d'identifiants (un essai par email), ni l'épuisement CPU (chaque
  login ou signup coûte ~50 ms d'Argon2). Par IP cliente et par fenêtre de
  15 min : au plus `AUTOME_LOGIN_IP_MAX_ATTEMPTS` (défaut 50) échecs de
  connexion — une connexion réussie ne compte pas, pour les bureaux derrière un
  même NAT — et `AUTOME_SIGNUP_IP_MAX_ATTEMPTS` (défaut 10) inscriptions ;
  au-delà, 429 + `Retry-After`. `0` désactive un quota. Les IPv6 sont comptées
  par préfixe `/64`. Compteurs partagés entre répliques comme celui des emails.
- **IP réelle derrière un proxy** : `X-Forwarded-For` n'est lu que si la
  requête vient d'un proxy listé dans `AUTOME_TRUSTED_PROXIES` (CIDR ou
  adresses, séparés par des virgules) ; l'IP retenue est la plus à droite de la
  chaîne qui ne soit pas un proxy de confiance. Sans proxy de confiance
  (défaut, cas de compose), l'adresse du pair TCP fait foi et l'en-tête est
  ignoré — un client ne peut pas choisir son IP. En k8s, l'API voit le
  contrôleur ingress : `k8s/config.yaml` fait confiance aux réseaux privés,
  **à restreindre au CIDR des pods de l'ingress**. Une plage trop large laisse
  les machines qui s'y trouvent choisir l'IP sous laquelle elles sont comptées ;
  une liste vide derrière un proxy compte tous les clients sous l'IP du proxy
  (tout le monde bloqué ensemble).
- Pas de plafond global : il bloquerait tous les utilisateurs au premier pic
  (déni de service offert à l'attaquant). Pour repérer une attaque distribuée
  sur beaucoup d'IP, surveiller
  `autome_http_requests_total{path="/auth/login",status="429"}` et les
  `401` de la même route sur `/metrics`. Données personnelles : voir
  ci-dessous.
- **Isolation par propriétaire** : un workflow appartient à son créateur, un run
  au propriétaire de son workflow. Les listes ne montrent que ses propres
  ressources ; lire, modifier, lancer ou supprimer la ressource d'autrui répond
  comme si elle n'existait pas (404, ou 204 pour `DELETE`).
- **Ids de workflow par utilisateur** : chaque compte a son propre espace d'ids
  (clé `(owner_id, id)`, migration `0014`). Deux comptes peuvent créer chacun
  un workflow `wf-draft` ; `POST /workflows` ne répond 409 que si l'id existe
  déjà **dans son propre espace** — l'existence des workflows d'autrui n'est
  jamais révélée. Les runs d'un workflow sont exécutés avec la version de
  l'espace de leur propriétaire.
- **Super-admin** (permission `*`) : travaille lui aussi dans son propre espace
  par défaut ; il vise celui d'un autre compte avec `?owner=<user_id>` sur
  `/workflows`, `/workflows/{id}` et `/workflows/{id}/runs` (et lit tout run
  par son id). `?owner=` vide désigne les workflows antérieurs à la migration
  `0010`, sans propriétaire. Un utilisateur ordinaire qui vise l'espace d'autrui
  reçoit 403, que la ressource existe ou non.

```bash
curl -s -H 'content-type: application/json' \
  -d '{"email":"me@example.com","password":"correct-horse"}' \
  http://localhost:8080/auth/signup
```

Base de dev seedée (`migrations/seeds/dev_seed.sql`) : compte super-admin
`dev@autome.local` / `autome-dev-password` (à ne jamais appliquer en prod).

### Données personnelles (adresses IP)

Une adresse IP est une donnée personnelle (RGPD : CJUE, *Breyer*, 2016). Le
traitement repose sur l'intérêt légitime de sécurité (art. 6.1.f, considérant 49 ;
art. 32) et suit ces règles :

- **finalité unique** : limiter les tentatives d'authentification ;
- **minimisation** : l'IP n'est jamais stockée en clair — le compteur est
  indexé par un HMAC-SHA256 de l'adresse, à clé secrète dérivée
  d'`ENCRYPTION_KEY` (un simple hachage se renverserait en énumérant les IPv4),
  lui-même haché en base ;
- **conservation** : 15 min (fenêtre), purge automatique ;
- **ni logs ni audit** : l'API ne journalise jamais l'IP cliente.

Hors du code d'Autome : l'ingress ou le load balancer journalisent en général
les IP de leur côté — leur durée de conservation est à régler et à documenter
avec le reste. Mentionner ce traitement dans la politique de confidentialité et
le registre des traitements ; le faire valider par le DPO ou un juriste avant
une mise en production réelle.

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
| L'API ou le worker s'arrête sur `NOT_ALLOWED - vhost  not found` | `QUEUE_URL` terminée par une barre seule (`…:5672/` = vhost vide) | Écrire le vhost : `amqp://user:pass@host:5672/%2f` |
| Logs `erreur de file … retry_in_ms=…` en continu côté worker | Broker indisponible | Le worker se reconnecte seul dès le retour du broker (`connexion RabbitMQ rétablie`) ; vérifier l'état de RabbitMQ |
| Logs `connexion impossible, nouvelle tentative service="rabbitmq"` au démarrage | Broker démarré mais pas encore à l'écoute | Normal pendant quelques secondes ; au-delà de `AUTOME_QUEUE_CONNECT_TIMEOUT_SECS`, vérifier `QUEUE_URL` et le broker |
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
