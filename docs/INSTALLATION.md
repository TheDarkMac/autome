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
| `AUTOME_WORKER_CONCURRENCY` | Worker : tâches consommatrices par process ; aussi le *prefetch* RabbitMQ (messages poussés d'avance au process) | `4` | selon charge |
| `AUTOME_RUN_TIMEOUT_SECS` | Worker : au-delà, une tentative encore `running` est close en `failed` (min. 60) | `3600` | > durée du plus long run |
| `AUTOME_RUN_LEASE_SECS` | Worker : bail d'un run en cours (min. 10) ; sans battement depuis ce délai (worker tué), le run est remis en file et repris par un autre worker | `60` | `60` |
| `AUTOME_SCHEDULER_TICK_SECS` | Worker : période de la boucle des déclencheurs `schedule` / `polling` (s, `0` = désactivée) | `10` | `10` |
| `AUTOME_SMTP_URL` | Relais SMTP des emails (mot de passe oublié, vérification de l'adresse) : `smtps://user:pass@hôte` (TLS, 465) ou `smtp://hôte:587?tls=required` (STARTTLS). Contient un mot de passe : à mettre dans un secret | *(non défini : liens journalisés en `dev`)* | **requis pour le mot de passe oublié** (sinon 503) et la vérification de l'email (sinon comptes créés vérifiés) |
| `AUTOME_EMAIL_VERIFICATION` | `required` : un compte inscrit ne se connecte qu'après avoir ouvert le lien reçu par email (dès qu'un envoi d'email est possible) ; `off` : comptes créés vérifiés (#231) | `required` | `required` |
| `AUTOME_MAIL_FROM` | Expéditeur des emails, ex. `Autome <no-reply@exemple.fr>` | *(non défini)* | obligatoire avec `AUTOME_SMTP_URL` |
| `AUTOME_CORS_ORIGINS` | Origines du **frontend** autorisées à appeler l'API depuis le navigateur (`scheme://hôte[:port]`, séparées par des virgules ; ni joker, ni chemin — refus de démarrer sinon). Vide = aucune | `http://localhost:5173,http://localhost:8081` | **obligatoire** si le frontend est servi sur une autre origine que l'API |
| `AUTOME_PUBLIC_URL` | URL publique du **frontend**, base des liens envoyés par email (jamais déduite de l'en-tête `Host`) | `http://localhost:5173` | obligatoire avec `AUTOME_SMTP_URL` |
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
son nom ou son IP à `AUTOME_EGRESS_ALLOWLIST`.

**Proxy sortant** : `HTTP_PROXY`, `HTTPS_PROXY` et `ALL_PROXY` (et leurs formes
en minuscules) sont **ignorés** par ces requêtes ; l'API et le worker
l'indiquent au démarrage. Derrière un proxy, c'est lui qui résoudrait l'hôte
cible, hors de portée du filtre : `http://localhost/` ou un nom interne
redeviendraient joignables (#233). Si le réseau impose de sortir par un proxy,
utilisez une passerelle transparente (NAT, egress gateway du maillage) : la
connexion part alors vers l'adresse déjà vérifiée.

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

Le worker ne sonde pas la file : il s'y abonne (`basic_consume`) et RabbitMQ
lui **pousse** les runs, au plus `AUTOME_WORKER_CONCURRENCY` non acquittés par
process (*prefetch*) ; un run publié démarre sans attendre de tour de boucle, et
une file vide ne coûte aucun aller-retour réseau. Le reste de la file revient
aux autres répliques. À l'arrêt, le worker se désabonne d'abord et rend au
broker les runs reçus d'avance mais pas commencés : les autres répliques les
prennent aussitôt, sans attendre la fin du drain. Dans l'interface de
gestion de RabbitMQ, la queue `runs` montre un consommateur par réplique, et
ses messages *Unacked* (au plus le prefetch par réplique) sont ceux en cours
ou reçus d'avance.

### Exécution des runs : au plus une fois

RabbitMQ livre chaque message **au moins une fois** : après un crash ou une
perte de connexion, un run peut être relivré, à n'importe quelle réplique du
worker. Pour que ses effets de bord (message Slack, e-mail, issue GitHub…) ne
se produisent pas deux fois, le worker **réclame** le run en base avant de
l'exécuter (`queued → running`, atomique) : une seule réplique gagne, les
livraisons dupliquées d'un run déjà pris ou terminé sont acquittées sans
ré-exécution.

Les runs ne se perdent pas entre l'API et la file : le run `queued` et son
message sont écrits **dans la même transaction** (table `run_outbox`, #239),
puis publiés. Si la publication échoue (broker coupé, process tué au mauvais
moment), le run est quand même accepté et toutes les 5 s un worker publie les
messages restés en attente ; un message publié deux fois est écarté par le
claim.

**Reprise des runs interrompus (#239).** Avant chaque nœud, le worker
l'enregistre comme *nœud en cours* (`next_node`) ; après, il enregistre le
*point de reprise* du moteur et efface le nœud en cours. Un nœud ne démarre
jamais sans ces deux écritures. Un run interrompu est remis en file et repris
par un autre worker à son point de reprise — les nœuds déjà exécutés ne sont
pas rejoués :

- **arrêt du worker** (déploiement, mise à l'échelle) : il s'arrête au
  prochain nœud, cf. C.5 ;
- **worker tué** (OOM, nœud perdu) : il bat toutes les `AUTOME_RUN_LEASE_SECS`
  / 4 ; un run sans battement depuis `AUTOME_RUN_LEASE_SECS` (défaut 60 s) est
  remis en file par un autre worker.

Si un nœud était **en cours** au moment de l'interruption, son effet est
incertain (le message Slack est peut-être parti). Il n'est rejoué que s'il est
sans effet (trigger, `if`, `switch`) ou si sa politique d'erreur autorise déjà
plusieurs tentatives (`"_error": { "max_attempts": 2 }` ou plus) ; sinon le run
est clos en `failed` avec un motif qui nomme le nœud — relancer le workflow si
besoin. Un run est aussi clos en `failed`, sans reprise, si son workflow a été
modifié depuis l'interruption (nœuds, paramètres ou arêtes ; déplacer un nœud
dans l'éditeur ne compte pas) ou après 10 tentatives. Chaque écriture du
worker est gardée par le numéro de tentative : un worker qui a perdu la main
(coupé du réseau puis revenu) n'écrit plus rien et s'arrête au prochain nœud.

Toutes les 60 s, chaque worker clôt aussi en `failed` les runs dont la
tentative est encore `running` au-delà de `AUTOME_RUN_TIMEOUT_SECS` (défaut
1 h), avec une erreur explicite. Réglez ce délai au-dessus de la durée du plus
long run légitime : sinon un run encore actif serait affiché en échec jusqu'à
ce que son état final le corrige.

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
  : le node échoue en demandant de reconnecter le compte. Un seul
  rafraîchissement à la fois par (utilisateur, provider), **toutes répliques
  confondues** : le worker prend le verrou de la ligne `connector_tokens`
  (`SELECT … FOR UPDATE`, attente bornée à 60 s) et relit le jeton avant
  d'appeler le provider — indispensable pour les providers qui font tourner le
  refresh token (chacun n'est présenté qu'une fois). Refus **définitif** du
  provider (`invalid_grant` : refresh token révoqué, expiré ou déjà consommé) :
  la credential est marquée invalide (`invalidated_at_ms`, `invalid_reason`,
  migration `0018`) et les runs suivants échouent **sans rappeler le
  provider** ; `GET /credentials` l'expose (`needs_reconnect`,
  `invalidated_at_ms` — le motif brut reste en base) et l'onglet Connecteurs
  affiche le badge « À reconnecter » ; reconnecter le compte
  (`/credentials/<provider>/authorize`) efface la marque. Une erreur transitoire (réseau, 5xx) ne marque rien : le run
  suivant retente.
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

Le premier build compile toutes les dépendances Rust (≈ 25 min sur 4 cœurs
pour l'API et le worker) ; les suivants ne recompilent que les crates du
workspace tant que `Cargo.toml` / `Cargo.lock` ne changent pas (couche
cargo-chef en cache, #219).

L'API et le worker y partagent **PostgreSQL** (`AUTOME_PERSISTENCE=postgres`)
et **RabbitMQ** (`AUTOME_QUEUE=rabbitmq`) ; l'API applique les migrations au
démarrage (`AUTOME_MIGRATE=true`). Les données survivent à un
`docker compose restart` (volume `pgdata`).

| Service | URL | Identifiants |
|---|---|---|
| API | http://localhost:8080 | — (`/livez`, `/readyz`, `/metrics`, `/openapi.json`) |
| Frontend | http://localhost:8081 | — |
| Grafana | http://localhost:3000 | `admin` / `admin` |
| Prometheus | http://localhost:9090 | — |
| RabbitMQ (UI) | http://localhost:15672 | `guest` / `guest` |
| PostgreSQL | `localhost:5432` | `postgres` / `postgres` |

Vérification rapide :

```bash
curl http://localhost:8080/readyz     # -> {"status":"ready","checks":{"postgres":"ok","rabbitmq":"ok"}}
curl http://localhost:8080/metrics    # -> métriques Prometheus de l'API
docker compose exec worker bash -c 'exec 3<>/dev/tcp/127.0.0.1/9091; printf "GET /metrics HTTP/1.0\r\n\r\n" >&3; cat <&3'
                                      # -> métriques du worker (port non publié)
curl http://localhost:8080/openapi.json  # -> spec OpenAPI 3 de l'API (publique)
docker compose ps                     # api et worker « healthy »
```

Vérification **de bout en bout** — compte jetable, workflow, run exécuté par
le worker jusqu'à `succeeded` (échoue si l'API et le worker ne partagent pas
la file) :

```bash
scripts/compose-smoke.sh
```

La CI le rejoue à chaque push et PR vers `main` (job *Smoke e2e*, #216) sur
les images construites depuis les Dockerfiles. Pour reproduire ce job en local
(images taggées `:ci`, sans frontend ni monitoring) :

```bash
export COMPOSE_FILE=docker-compose.yml:docker/compose.ci.yml
docker buildx bake -f docker-compose.yml -f docker/compose.ci.yml --load api worker
docker compose up --no-build --wait api worker
scripts/compose-smoke.sh
docker compose down --volumes
```

Compte super-admin de dev (optionnel) : appliquer le seed **après** le premier
démarrage de l'API (migrations appliquées), puis se connecter avec
`dev@autome.local` / `autome-dev-password` — ou promouvoir son propre compte
(`docker compose exec api autome-api promote-admin <email>`, cf.
« Administration des utilisateurs ») :

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
L'URL du backend est injectée au build via `VITE_API_URL`. Le frontend et
l'API étant sur deux origines, l'API doit autoriser celle du frontend
(`AUTOME_CORS_ORIGINS` ; en `dev`, `:5173` et `:8081` le sont par défaut).

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
  dans l'image ; vide = sondes désactivées). Le même port sert `GET /metrics`
  (cf. C.6).
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

**Arrêt borné du worker (#200, #239).** Sur `SIGTERM` / `SIGINT`, le worker
cesse de prendre de nouveaux runs, `/readyz` répond 503 `draining` (`/livez`
reste 200), et chaque run en cours **s'arrête au prochain nœud** : son point
de reprise est enregistré, il est remis en file et un autre worker le reprend
sans rien rejouer. Un nœud long a `AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS`
(défaut **25 s**) pour se terminer. À l'échéance, les runs encore dans un
nœud sont remis en file tout de suite ; le worker qui les reprend applique la
règle du nœud en cours (rejoué seulement s'il est rejouable, sinon `failed` :
livraison *au plus une fois*). Un run créé avant #239 (sans message dans
l'outbox) ne peut pas être remis en file : il est clos en `failed` — « worker
arrêté pendant l'exécution : run interrompu (non rejoué, à relancer) ».
Périodes de grâce : `terminationGracePeriodSeconds: 35` (`k8s/worker.yaml`),
`stop_grace_period: 35s` (compose). **Nœuds longs** (appel HTTP lent,
boucle) : augmenter ensemble `AUTOME_WORKER_SHUTDOWN_TIMEOUT_SECS` et la
période de grâce (grâce = délai + ~10 s), sinon un déploiement interrompra ces
nœuds en cours.

### C.6 Monitoring

Le namespace inclut **Prometheus** (scrape de l'API et de chaque worker) et
**Grafana** (datasource + dashboard *Autome — Overview* provisionnés) :

```bash
kubectl -n autome port-forward svc/grafana 3000:3000     # http://localhost:3000 (admin/admin)
kubectl -n autome port-forward svc/prometheus 9090:9090  # http://localhost:9090
```

**Métriques du worker (#235)** : `GET /metrics` sur le port des sondes (`9091`),
servi aussi pendant l'arrêt. Prometheus découvre **chaque réplique** par DNS
(job `autome-worker`, `dns_sd_configs` sur le nom `worker` : DNS de Docker en
compose, Service headless `worker` en k8s) ; un label `instance` par pod.

| Série | Type | Lecture |
|---|---|---|
| `autome_worker_deliveries_total{outcome}` | counter | messages de la file traités : `ack`, `retry`, `dead_letter`, `deduplicated` |
| `autome_worker_queue_errors_total` | counter | poll / ack / nack en échec (broker qui décroche) |
| `autome_worker_runs_total{status}` | counter | runs exécutés jusqu'au bout par ce worker : `succeeded`, `failed` (une redélivrance acquittée sans exécution n'en est pas un) |
| `autome_worker_run_duration_seconds{status}` | histogram | durée entre le claim et l'état final (buckets 0,1 s → 1 h) |
| `autome_worker_runs_in_flight` | gauge | runs en cours sur ce worker |
| `autome_worker_orphan_runs_reaped_total` | counter | runs interrompus clos par le balayage des orphelins |
| `autome_worker_runs_requeued_total{reason}` | counter | runs remis en file : `shutdown` (arrêt de ce worker), `stale` (sans battement, remis par ce worker) |
| `autome_worker_runs_resumed_total` | counter | runs repris à leur point de reprise par ce worker |
| `autome_worker_outbox_relayed_total` | counter | messages de run publiés par le relais de l'outbox (publication du dispatcher perdue) |
| `autome_worker_outbox_errors_total` | counter | passes du relais ou de la remise en file en échec (broker ou base indisponible) |
| `autome_worker_trigger_runs_total{kind,result}` | counter | runs des déclencheurs `schedule` / `polling` : `dispatched`, ou `failed` (déclenchement perdu) |
| `autome_worker_trigger_missed_total` | counter | échéances `schedule` sautées (plus de 5 min de retard) |
| `autome_worker_trigger_poll_errors_total` | counter | polls en échec (réseau, statut HTTP, réponse illisible) |
| `autome_worker_trigger_tick_errors_total` | counter | passes de planification sans accès aux échéances |
| `autome_worker_draining` | gauge | `1` pendant l'arrêt |

Toutes les séries existent dès le démarrage (à zéro) et leurs labels sont des
valeurs fixes : **aucun id de run, de workflow ni d'utilisateur**. Comme
`/metrics` de l'API, l'endpoint n'est pas authentifié ; le port `9091` ne doit
pas sortir du réseau interne (non publié en compose, Service headless sans
Ingress en k8s). Exemples d'alertes : `rate(autome_worker_queue_errors_total[5m]) > 0`,
`sum(rate(autome_worker_runs_total{status="failed"}[15m])) / sum(rate(autome_worker_runs_total[15m])) > 0.2`,
`increase(autome_worker_trigger_runs_total{result="failed"}[15m]) > 0`,
`increase(autome_worker_runs_requeued_total{reason="stale"}[15m]) > 0` (workers
tués), `increase(autome_worker_outbox_errors_total[5m]) > 0`.

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

## Déclencheurs webhook (#202)

Chaque nœud `trigger_type: "webhook"` d'un workflow enregistré reçoit une
**URL secrète** : `POST <API>/hooks/<token>` (64 caractères hexadécimaux),
**sans** jeton Bearer — connaître l'URL suffit à déclencher le workflow.

- **Obtenir l'URL** : `GET /workflows/{id}/webhooks` (Bearer, permission
  `runs.execute`) renvoie, pour chaque nœud webhook, `path` (`/hooks/…`),
  `method` et `signing_secret` ; le panneau du nœud dans l'éditeur l'affiche
  aussi (copie, secret, rotation). L'URL est créée à la première lecture.
- **Rotation** : `POST /workflows/{id}/webhooks/{node_id}/rotate` — nouvelle
  URL et nouveau secret, les anciens cessent aussitôt de fonctionner. Retirer
  le nœud du workflow ou supprimer le workflow supprime aussi l'URL.
- **Paramètres du nœud** : `method` (défaut `POST`, sinon 405) ;
  `signature: "github"` exige un `X-Hub-Signature-256` valide calculé avec le
  `signing_secret` (à coller dans le champ « Secret » du webhook GitHub, type
  de contenu `application/json`), sinon 401 ; `event` (ex. `issues`) : les
  autres valeurs de `X-GitHub-Event` (dont `ping`) sont acquittées **204**
  sans run. `path` n'est qu'un libellé.
- **Réponses** : 202 `{ run_id, status }` ; 404 pour une URL inconnue (ou
  dont le nœud/workflow a disparu) ; 413 au-delà de 1 Mio ; 429 +
  `Retry-After` au-delà de `AUTOME_WEBHOOK_MAX_PER_WINDOW` requêtes
  (défaut 1000) par URL et par fenêtre de 15 min, compteur partagé entre
  répliques en `postgres`.
- **Dans le workflow**, le nœud trigger produit
  `{ node_id, method, query, headers, body }` : corps JSON parsé (sinon texte,
  `null` si vide), en-têtes en minuscules **sans** `authorization`, `cookie`,
  signatures ni en-têtes d'adresse IP. Exemple :
  `{{ hook.body.issue.title }}` dans les params d'une action.

```bash
curl -X POST "$API/hooks/<token>" -H 'Content-Type: application/json' \
  -d '{"issue": {"title": "Bug"}}'
```

**Secrets** : la base ne contient que le sel de chaque URL et le SHA-256 du
token ; token et secret de signature sont dérivés d'`ENCRYPTION_KEY` (table
`webhook_endpoints`, migration `0016`). Changer `ENCRYPTION_KEY` change donc
toutes les URL (à redistribuer aux émetteurs). Les journaux affichent
`/hooks/{token}`, jamais le token.

## Déclencheurs planifiés : cron, intervalle, polling (#227)

Les nœuds `trigger_type: "schedule"` et `"polling"` d'un workflow enregistré
démarrent des runs sans intervention : c'est le **worker** qui les déclenche
(boucle toutes les `AUTOME_SCHEDULER_TICK_SECS`, défaut 10 s). Toutes les
répliques font tourner la boucle ; les échéances sont partagées en base
(`trigger_schedules`, migration `0019`) et chacune n'est déclenchée **qu'une
fois** (compare-and-set sur une révision). PostgreSQL est requis : en
`AUTOME_PERSISTENCE=memory`, l'API et le worker ne partagent rien.

- **`schedule`** : `cron` (5 champs, **UTC** : `*/15 * * * *`, `0 9 * * 1-5`,
  `0 0 1 * *` ; plages, pas, listes ; jour de semaine 0–7, 0 et 7 = dimanche ;
  si jour du mois et jour de semaine sont tous deux restreints, l'un **ou**
  l'autre suffit) **ou** `interval_seconds` (5 à 2 678 400). `timezone` :
  seul `UTC` est accepté. Payload du nœud :
  `{ node_id, scheduled_at, scheduled_at_ms }`.
- **`polling`** : `GET url` toutes les `interval_seconds` (défaut 60, min. 5)
  par le client HTTP filtré anti-SSRF (`AUTOME_EGRESS_ALLOWLIST` pour une cible
  interne). Les éléments du tableau JSON (la réponse, ou le chemin pointé
  `items_path`, ex. `data.items`, 1000 premiers) sont identifiés par `key`
  (défaut `id`) ; **un run par nouvel élément** (au plus 100 par poll, la suite
  au poll suivant), payload `{ node_id, item }`. Le **premier poll sert de
  référence** : les éléments déjà présents ne déclenchent rien. Réponse non
  2xx, non JSON ou sans tableau : journalisée, nouvel essai à l'échéance
  suivante. Pas d'authentification de la requête pour l'instant.
- **Validation** à l'enregistrement : `POST` / `PUT /workflows` répondent 400
  en nommant le nœud (cron invalide ou impossible comme `0 0 30 2 *`, `cron` et
  `interval_seconds` ensemble, fuseau non UTC, URL non http(s)…). Modifier les
  paramètres d'un nœud recalcule son échéance (et remet à zéro la référence
  d'un polling) ; retirer le nœud ou supprimer le workflow arrête les runs.
- **Garanties** : déclenchement **au plus une fois** — un worker tué entre
  l'avancée de l'échéance et l'envoi du run perd ce déclenchement. Une
  échéance `schedule` en retard de plus de 5 min (workers arrêtés) est
  **sautée** (journal `échéance manquée`), sans rafale de rattrapage. La
  précision est celle de la période de la boucle.

## Authentification (comptes & jetons)

L'API émet ses propres JWT (HS256, signés avec `JWT_SECRET`). Les endpoints de
comptes sont publics ; tout le reste exige `Authorization: Bearer <access_token>`.

| Méthode | Path | Body | Réponse |
|---|---|---|---|
| POST | `/auth/signup` | `{"email","password"}` | 201 + paire de jetons (vérification désactivée) · 202 `{"email","verification_required":true}` sans jetons, lien de vérification envoyé · 400 saisie invalide · 409 email déjà pris |
| POST | `/auth/login` | `{"email","password"}` | 200 + paire de jetons · 401 identifiants invalides · 403 `account disabled` (compte suspendu) ou `email not verified` · 429 trop d'échecs (`Retry-After`) |
| POST | `/auth/refresh` | `{"refresh_token"}` | 200 + nouvelle paire · 401 refresh invalide, expiré ou déjà utilisé |
| POST | `/auth/logout` | `{"refresh_token"}` (+ `Authorization: Bearer <access_token>` facultatif) | 204 session fermée (aussi si déjà fermée) · 401 refresh invalide ou expiré |
| POST | `/auth/logout-all` | — (`Authorization: Bearer <access_token>` requis) | 204 toutes les sessions fermées · 401 access absent, invalide ou déjà révoqué |
| POST | `/auth/password` | `{"current_password","new_password"}` (Bearer requis) | 200 + nouvelle paire · 400 nouveau mot de passe invalide ou identique · 403 mot de passe actuel incorrect · 409 changé entre-temps · 429 trop d'essais |
| POST | `/auth/password/forgot` | `{"email"}` | 202 demande prise en compte (que le compte existe ou non) · 400 email invalide · 429 trop de demandes · 503 aucun envoi d'email configuré |
| POST | `/auth/password/reset` | `{"token","new_password"}` | 204 mot de passe remplacé, sessions fermées · 400 lien invalide, expiré ou déjà utilisé, ou mot de passe invalide · 503 non configuré |
| POST | `/auth/email/verify` | `{"token"}` | 204 adresse vérifiée · 400 lien invalide ou expiré · 503 vérification désactivée |
| POST | `/auth/email/resend` | `{"email"}` | 202 demande prise en compte (que le compte existe ou non) · 400 email invalide · 429 trop de demandes · 503 vérification désactivée |
| GET | `/auth/me` | — (Bearer requis) | 200 `{"id","email","role","permissions"}` · 401 session invalide ou compte suspendu |
| DELETE | `/auth/me` | `{"current_password"}` (Bearer requis) | 204 compte et données effacés, sessions fermées · 401 session invalide ou compte suspendu · 403 mot de passe actuel incorrect · 409 compte administrateur (`*`) · 429 trop d'essais |

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
  frontend ; audit `auth.password_change`.
- **Suppression du compte** (#229) : `DELETE /auth/me` efface définitivement
  le compte et ses données (voir « Suppression de compte (droit à
  l'effacement) » plus bas). Elle exige le mot de passe actuel, avec le même
  compteur d'essais que le changement de mot de passe (403 s'il est faux, 429
  au-delà). Un compte qui détient `*` est refusé (409) : un autre
  administrateur doit d'abord lui retirer ce rôle, si bien que l'instance n'est
  jamais laissée sans administrateur par ce chemin. Onglet « Compte » du
  frontend (mot de passe + email retapé) ; audit `auth.account_delete` sous le
  pseudonyme du compte.
- **Mot de passe oublié** (#204) : « Mot de passe oublié ? » sur la page de
  connexion appelle `/auth/password/forgot`, qui répond **toujours 202** : la
  recherche du compte et l'envoi de l'email ont lieu en tâche de fond, si bien
  que ni le code ni la durée de la réponse ne révèlent si l'adresse a un
  compte. L'email contient un lien `AUTOME_PUBLIC_URL/#reset=<jeton>` : le
  jeton est dans le **fragment**, que le navigateur n'envoie jamais au serveur
  du frontend (journaux d'accès) ni dans le `Referer` ; le frontend le retire
  aussitôt de la barre d'adresse. Le jeton est **sans état** : HMAC (clé
  dérivée d'`ENCRYPTION_KEY`) de l'identifiant du compte, de l'expiration et du
  hash **actuel** du mot de passe — rien n'est stocké, il expire après
  **30 min**, et il ne sert qu'**une fois** : la réinitialisation change le
  hash (écriture conditionnelle, deux utilisations simultanées : une seule
  réussit), comme tout changement de mot de passe, ce qui invalide les liens
  en circulation. Changer `ENCRYPTION_KEY` les invalide aussi. Après
  réinitialisation, **toutes les sessions sont fermées** et le blocage de
  connexion du compte est levé ; l'utilisateur se reconnecte. Quotas : 3
  demandes par adresse et `AUTOME_RESET_IP_MAX_ATTEMPTS` (défaut 10) par IP et
  par fenêtre de 15 min, puis 429 (pas d'inondation d'une boîte). Audit
  `auth.password_reset_request` / `auth.password_reset`. Envoi SMTP
  (`AUTOME_SMTP_URL`, `AUTOME_MAIL_FROM`) ; un échec d'envoi est journalisé
  (`envoi du lien de réinitialisation impossible`) sans changer la réponse.
  **Sans SMTP** : en `dev` (cas de compose), le contenu de l'email — lien
  compris — est écrit dans les journaux de l'API
  (`docker compose logs api | grep reset=`) ; en `prod`, la fonction est
  désactivée (503) et le frontend affiche « pas disponible sur ce serveur ».
- **Vérification de l'email** (#231) : dès qu'un envoi d'email est possible
  (SMTP, ou journal en `dev`) et sauf `AUTOME_EMAIL_VERIFICATION=off`,
  `/auth/signup` crée un compte **non vérifié**, lui envoie en tâche de fond un
  lien `AUTOME_PUBLIC_URL/#verify=<jeton>` et répond **202 sans jetons** ; le
  frontend affiche « Vérifiez votre boîte email ». Tant que le lien n'est pas
  ouvert, la connexion répond **403 `email not verified`** — seulement avec le
  bon mot de passe, sinon 401 comme d'habitude. Le jeton est sans état, comme
  celui du mot de passe oublié, mais sous une clé distincte (un lien de
  réinitialisation n'est jamais pris pour un lien de vérification) et lié à
  l'**email** du compte ; il vaut **24 h** et reste accepté s'il est rouvert.
  `/auth/email/resend` renvoie un lien : toujours 202 (aucun email si le compte
  n'existe pas, est déjà vérifié ou suspendu), 3 demandes par adresse et
  `AUTOME_VERIFY_IP_MAX_ATTEMPTS` (défaut 10) par IP et par fenêtre de 15 min.
  Une **réinitialisation du mot de passe** réussie vérifie aussi l'adresse :
  elle prouve la même possession de la boîte, si bien qu'une adresse
  « réservée » par un tiers qui ne la possède pas se récupère par « Mot de
  passe oublié ». Les comptes antérieurs à la migration `0020` sont tenus pour
  vérifiés. Sans envoi d'email possible (`prod` sans SMTP), les comptes sont
  créés vérifiés et les deux routes répondent 503. En `dev` sans SMTP (cas de
  compose), le lien est dans les journaux (`docker compose logs api | grep
  verify=`) ou l'adresse se vérifie depuis l'infrastructure :
  `docker compose exec api autome-api verify-email <email>`. Audit
  `auth.email_verify_request` / `auth.email_verify`.
- Mot de passe : 8 à 1024 caractères, haché en **Argon2id**. L'email est
  normalisé (trim + minuscules).
- Un compte créé par `signup` reçoit le rôle **editor** (CRUD workflows, runs,
  credentials) — pas d'administration (voir « Administration des utilisateurs »).
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
  même NAT —, `AUTOME_SIGNUP_IP_MAX_ATTEMPTS` (défaut 10) inscriptions et
  `AUTOME_RESET_IP_MAX_ATTEMPTS` (défaut 10) demandes de réinitialisation et
  `AUTOME_VERIFY_IP_MAX_ATTEMPTS` (défaut 10) renvois du lien de vérification ;
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

### Administration des utilisateurs

Onglet « Administration » du frontend, visible des seuls comptes qui détiennent
`users.admin` (ou `*`) ; API `/admin/users` (Bearer + `users.admin`, #208) :

| Méthode | Path | Body | Réponse |
|---|---|---|---|
| GET | `/admin/users?q=&offset=&limit=` | — | 200 `{"items","total","offset","limit"}` — comptes du plus ancien au plus récent, `q` = partie de l'email, `limit` ≤ 200 (défaut 50) |
| GET | `/admin/users/{id}` | — | 200 compte · 404 inconnu |
| PUT | `/admin/users/{id}/role` | `{"role":"admin"\|"editor"\|"viewer"}` | 200 compte · 400 rôle inconnu · 403 · 404 · 409 son propre rôle |
| POST | `/admin/users/{id}/disable` | — | 200 compte suspendu, sessions fermées · 403 · 404 · 409 son propre compte |
| POST | `/admin/users/{id}/enable` | — | 200 compte réactivé · 403 · 404 |
| POST | `/admin/users/{id}/logout-all` | — | 204 sessions du compte fermées · 403 · 404 |
| POST | `/admin/users/{id}/verify-email` | — | 200 compte, adresse vérifiée · 403 · 404 |
| DELETE | `/admin/users/{id}` | — | 204 compte et données effacés · 403 · 404 · 409 son propre compte |

Un compte vaut `{"id","email","role","permissions","disabled","email_verified","created_at_ms"}`
(jamais le hash du mot de passe). Règles :

- Le **rôle** n'est qu'une vue des permissions : `admin` = `*`, `editor` et
  `viewer` = les jeux standard, `custom` pour tout autre jeu. En changer
  remplace **toutes** les permissions du compte (une transaction) ; l'effet est
  immédiat, sans reconnexion.
- **Pas d'escalade** : attribuer le rôle `admin`, ou modifier, suspendre ou
  déconnecter un compte qui détient `*`, exige soi-même `*` (403 sinon) — un
  compte qui n'a que `users.admin` gère les autres comptes mais ne crée pas de
  super-admin.
- **Pas de verrouillage** : un administrateur ne change pas son propre rôle,
  ne se suspend pas et ne se supprime pas lui-même par ces routes (409).
- **Suspension** : réversible ; le compte garde ses workflows, runs et
  permissions. Toutes ses sessions sont fermées (version de session), sa
  connexion répond 403 « account disabled » — seulement avec le bon mot de
  passe, sinon 401 comme d'habitude — et aucun lien de réinitialisation ne lui
  est envoyé. Une connexion simultanée à la suspension ne peut pas en
  réchapper : le drapeau est écrit avant la fermeture des sessions et relu par
  la connexion après lecture de la version. Réactiver ne rouvre pas les
  anciennes sessions.
- **Suppression** (#229) : définitive, contrairement à la suspension — le
  compte et toutes ses données sont effacés (voir « Suppression de compte
  (droit à l'effacement) »), par exemple pour une demande reçue hors de
  l'application. Bouton « Supprimer » de l'onglet, confirmé.
- **Vérification de l'email** (#231) : à la place du titulaire (lien perdu,
  envoi d'emails en panne), l'adresse actuelle du compte est marquée vérifiée
  et sa connexion devient possible. Badge « Email non vérifié » et bouton
  « Vérifier l'email » de l'onglet, confirmé — à réserver aux adresses dont on
  sait qu'elles appartiennent au titulaire.
- Chaque action est auditée sous l'administrateur, ressource = compte visé :
  `admin.users_list`, `admin.user_get`, `admin.user_role` (avec `from`/`to`),
  `admin.user_disable`, `admin.user_enable`, `admin.user_logout_all`,
  `admin.user_verify_email`, `admin.user_delete` (ressource = pseudonyme du compte effacé, avec le nombre
  de workflows, runs et tokens supprimés) ; les refus (403/409) sont audités
  `Denied`.
- Pas de création de compte par un administrateur : un compte s'inscrit
  lui-même (`/auth/signup`), puis un administrateur ajuste son rôle.

**Premier administrateur** : l'API n'en crée jamais (aucune promotion par
HTTP). Inscrire le compte, puis le promouvoir depuis l'infrastructure — la
commande lit `DATABASE_URL` et exige `AUTOME_PERSISTENCE=postgres` :

```bash
docker compose exec api autome-api promote-admin admin@example.com
```

```bash
kubectl -n autome exec deploy/api -- autome-api promote-admin admin@example.com
```

Elle donne le rôle `admin` (`*`), réactive le compte s'il était suspendu et
vérifie son adresse email — c'est aussi le recours si plus aucun
administrateur n'a accès. Audit `admin.user_promote` sous l'acteur `cli`. Email
inconnu : code de sortie non nul et message `aucun compte pour …`.

`autome-api verify-email <email>` (mêmes prérequis) vérifie seulement
l'adresse d'un compte, sans toucher à ses droits — envoi d'emails en panne,
tests de bout en bout (`scripts/compose-smoke.sh`). Audit
`admin.user_verify_email` sous l'acteur `cli`.

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

### Suppression de compte (droit à l'effacement)

RGPD, art. 17 (#229). L'utilisateur supprime son compte depuis l'onglet
« Compte » (`DELETE /auth/me`) ; un administrateur peut le faire pour lui
(`DELETE /admin/users/{id}`), par exemple sur une demande reçue par email.

**Effacé**, en une transaction PostgreSQL (un échec n'efface rien) :

| Donnée | Table(s) |
|---|---|
| Compte : email, hash du mot de passe | `users` (et `credentials`, 0004, par cascade) |
| Permissions | `user_permissions` |
| Workflows de son espace, toutes versions | `workflows`, `workflow_versions` |
| URL webhook et échéances planifiées de ces workflows | `webhook_endpoints`, `trigger_schedules` (cascade) |
| Runs dont il est propriétaire et sorties des nœuds | `runs`, `run_steps` |
| Tokens OAuth des connecteurs | `connector_tokens` |

**Pseudonymisé, pas supprimé** : le journal d'audit (`audit_events`), qui trace
aussi les actions des administrateurs et l'effacement lui-même. Les
événements dont l'acteur ou la ressource est l'id du compte **ou son email**
(échecs de connexion, demandes de réinitialisation) reçoivent un pseudonyme
aléatoire `erased:<16 hex>`, propre à l'effacement : les événements du compte
restent groupés, mais plus rien ne les relie à la personne.

**Conservé** : la ligne `session_versions` du compte (un id aléatoire et un
entier ; la supprimer revaliderait ses anciens jetons) et les `jti` révoqués.
Les compteurs anti-bruteforce du compte (déjà hachés) sont remis à zéro.

Toutes les sessions du compte sont fermées **avant** l'effacement. Un run en
cours d'exécution n'est pas recréé : le worker n'écrit l'état final d'un run
que s'il existe encore (`RunStateStore::finish`) et abandonne sinon (log
`run effacé pendant son exécution`). L'email redevient libre pour une
nouvelle inscription.

**Limites**, à traiter hors de l'application ou plus tard :

- les tokens OAuth sont supprimés chez Autome mais **pas révoqués chez le
  provider** (Slack, Google, Notion, GitHub) : l'utilisateur peut retirer
  l'accès depuis le compte du service ;
- un message déjà en file pour un run effacé est écarté par le worker en
  *dead letter* (`run introuvable`) ; son contenu (payload du déclencheur) reste
  dans la DLQ RabbitMQ jusqu'à sa purge ;
- sauvegardes de la base et journaux de l'ingress : selon leur propre durée de
  conservation ;
- pas d'export des données (portabilité, art. 20) ni de délai de grâce : la
  suppression est immédiate.

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
| « Mot de passe oublié » répond 503 / « pas disponible sur ce serveur » | Profil `prod` sans `AUTOME_SMTP_URL` | Définir `AUTOME_SMTP_URL`, `AUTOME_MAIL_FROM` et `AUTOME_PUBLIC_URL` |
| Aucun email de réinitialisation reçu | Relais SMTP refusé/injoignable (log `envoi du lien de réinitialisation impossible`), quota atteint, ou adresse sans compte | Vérifier `AUTOME_SMTP_URL` et les journaux de l'API ; en `dev` sans SMTP, le lien est dans les journaux |
| Connexion refusée « adresse email pas encore vérifiée » (403 `email not verified`) | Lien de vérification pas encore ouvert (#231) | Ouvrir le lien reçu ou cliquer « Renvoyer le lien » ; en `dev` sans SMTP, le lien est dans les journaux de l'API ; sinon `autome-api verify-email <email>` ou l'onglet Administration |
| Console du navigateur : `blocked by CORS policy` / préflight `OPTIONS` en 405 ; l'interface ne charge rien | Origine du frontend absente de `AUTOME_CORS_ORIGINS` (aucune par défaut en `prod`) | Ajouter l'origine exacte du frontend (schéma + hôte + port) à `AUTOME_CORS_ORIGINS` et redémarrer l'API |
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
