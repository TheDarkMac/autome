# ADR 0001 — Choix du framework backend : axum vs actix-web

- **Statut** : Accepté
- **Date** : 2026-05-16
- **Feature** : F1.2 (cf. [ROADMAP](../../ROADMAP.md)) — issue #3
- **Décideurs** : équipe Autome
- **Contexte ADR** : confirme la décision provisoire de l'[ADR initial d'architecture](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial)

---

## 1. Contexte

Le backend d'Autome est en Rust (cf. `CLAUDE.md`). Il expose une API
HTTP/REST (CRUD workflows, déclenchement de runs, ingress webhooks) et doit
s'intégrer proprement avec le reste de l'architecture :

- exécution **asynchrone** massivement concurrente (workers, file de messages) ;
- middlewares transverses (auth JWT, TLS, logs structurés, métriques
  Prometheus, rate-limit) ;
- séparation nette **API / moteur** : le framework web ne doit pas contaminer
  le cœur métier (`core`, `engine`).

Deux frameworks dominent l'écosystème Rust : **axum** et **actix-web**. La
contrainte `CLAUDE.md` autorise explicitement l'un ou l'autre. Cet ADR tranche.

## 2. Options envisagées

### Option A — axum

- Construit par l'équipe Tokio, au-dessus de `hyper` + `tower` + `tower-http`.
- Extracteurs typés, handlers = simples fonctions `async`.
- Middlewares = couches `tower` **réutilisables** entre l'API et les workers
  (mêmes briques pour timeout, retry, concurrency-limit, tracing).
- Modèle de concurrence : `Send` futures sur le runtime Tokio standard.

### Option B — actix-web

- Très mature, performances de pointe historiques (benchmarks TechEmpower).
- Modèle d'acteurs (`actix`) optionnel ; runtime propre (`actix-rt`).
- Écosystème de middlewares riche et éprouvé.

### Option C — autre (warp, rocket, poem)

- Écartée : warp (DSL de filtres peu lisible à grande échelle), Rocket
  (historiquement nightly, intégration Tokio moins naturelle), poem (moins
  répandu, écosystème de recrutement plus mince).

## 3. Critères de décision

| Critère | Poids | axum | actix-web |
|---|---|---|---|
| Intégration écosystème Tokio (file, `sqlx`, workers) | ⭐⭐⭐ | Excellente (même stack) | Bonne (runtime distinct possible) |
| Réutilisation middlewares API ↔ worker (`tower`) | ⭐⭐⭐ | Native (couches `tower`) | Spécifique au framework |
| Courbe d'apprentissage / lisibilité | ⭐⭐ | Faible (handlers = fn async) | Modérée (extractors + acteurs) |
| Performance brute | ⭐ | Très bonne | Excellente |
| Maturité & stabilité d'API | ⭐⭐ | Stable, gouvernance Tokio | Très mature |
| Observabilité (`tracing`, métriques) | ⭐⭐ | Intégration `tracing` de 1ʳᵉ classe | Bonne |
| Cohérence avec l'ADR d'architecture | ⭐⭐ | Alignée | Divergente |

## 4. Décision

**Nous adoptons `axum`** comme framework HTTP du backend Autome.

Raisons déterminantes :

1. **Une seule stack asynchrone.** `engine`, `worker`, le client de file et
   `sqlx` vivent déjà sur Tokio/`tower`. axum évite un second modèle de
   runtime/concurrence et les frictions associées.
2. **Middlewares mutualisés.** Les politiques transverses du moteur (timeout
   et limite de concurrence par node — F2.5/F2.7, retry/backpressure — F5.5)
   et celles de l'API (auth, rate-limit, tracing) reposent sur les **mêmes
   couches `tower`**, ce qui réduit la duplication et la surface de bugs.
3. **Lisibilité & maintenabilité.** Handlers = fonctions `async` à
   extracteurs typés : onboarding plus rapide, conforme à l'objectif de
   projet « maintenable et professionnel ».
4. **Observabilité.** Intégration `tracing` native, essentielle pour le
   traçage des runs et l'objectif d'observabilité de l'architecture.

La performance d'actix-web n'est pas un facteur décisif : le goulot
d'étranglement attendu est l'**exécution des workflows** (I/O réseau des
connecteurs, base, file), pas le framework HTTP. axum est largement
suffisant et scalable horizontalement.

## 5. Conséquences

**Positives**
- Stack homogène Tokio/`tower`/`hyper` sur tout le backend.
- Briques middleware réutilisables entre `api` et `worker`.
- Découplage préservé : `axum` reste confiné au crate `api` ; `core` et
  `engine` n'en dépendent pas (cf. F1.3).

**Négatives / risques**
- API d'axum encore en évolution (montées de version possibles) → mitigé par
  l'isolation dans le seul crate `api` et le verrouillage de version dans le
  workspace.
- Légère perte de performance brute vs actix-web → jugée non significative au
  regard du profil de charge réel (I/O-bound).

**Suites**
- F1.3 : `axum` déclaré comme dépendance du **seul** crate `api`.
- F6.1/F6.4 : auth JWT et TLS implémentés via couches `tower-http`.
- Si un service interne devient CPU/perf-critique, gRPC (`tonic`, lui aussi
  sur `tower`) reste une option cohérente sans rupture de stack.

## 6. Références

- [docs/ARCHITECTURE.md](../ARCHITECTURE.md) — ADR initial (axum « à confirmer F1.2 »)
- axum — <https://github.com/tokio-rs/axum>
- actix-web — <https://actix.rs/>
- `tower` (middlewares composables) — <https://github.com/tower-rs/tower>
