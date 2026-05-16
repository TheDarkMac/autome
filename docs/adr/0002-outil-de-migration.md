# ADR 0002 — Outil de migration de base de données

- **Statut** : Accepté
- **Date** : 2026-05-16
- **Feature** : F1.5.a (cf. [ROADMAP](../../ROADMAP.md)) — issue #9
- **Décideurs** : équipe Autome
- **Contexte ADR** : précise l'[ADR initial d'architecture](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial) (« PostgreSQL + `sqlx` »)

---

## 1. Contexte

Le backend persiste workflows, runs et credentials dans PostgreSQL
(ARCHITECTURE.md §6). Il faut un mécanisme de **migrations versionnées**,
reproductible en dev/CI/prod, cohérent avec le reste de la stack Rust.

## 2. Options

| Outil | Modèle | Remarques |
|---|---|---|
| **`sqlx` + `sqlx-cli`** | SQL pur, fichiers `NNNN_desc.sql`, table `_sqlx_migrations` | Même crate que l'accès données (déjà retenu archi), migrations embarquables au binaire (`sqlx::migrate!`), vérif requêtes à la compilation. |
| `refinery` | SQL ou Rust, embarqué | Bon outil mais ajoute une dépendance distincte de la couche d'accès. |
| `diesel` (+ `diesel_migrations`) | ORM + DSL | Impose l'ORM Diesel ; incompatible avec le choix `sqlx` (async, SQL explicite). |

## 3. Décision

**Nous adoptons `sqlx` + `sqlx-cli`**, migrations en **SQL pur** sous
`migrations/` au format `NNNN_description.sql`.

Raisons :

1. **Cohérence stack.** `sqlx` est déjà la couche d'accès données retenue
   (ADR archi) : un seul outil pour migrer *et* requêter, une seule table
   de suivi (`_sqlx_migrations`).
2. **SQL explicite & auditable.** Schéma lisible, revu en PR, rejouable
   avec `psql` sans outillage Rust (utile en CI et en prod managée).
3. **Embarquable.** `sqlx::migrate!("./migrations")` permet d'appliquer les
   migrations au démarrage d'un binaire (api/worker) — étape branchée plus
   tard (F1.6/F7).

## 4. Conséquences

- `sqlx-cli` est l'outil de référence (`sqlx migrate add/run`). Tant qu'il
  n'est pas installé en environnement, les migrations restent **applicables
  via `psql`** (même ordre lexicographique des fichiers).
- Convention de nommage : `migrations/NNNN_snom.sql`, numérotation à 4
  chiffres, **append-only** (pas de réécriture d'une migration livrée ; une
  correction = une nouvelle migration).
- Le seed de dev n'est **pas** une migration : `migrations/seeds/dev_seed.sql`,
  appliqué explicitement hors du flux de migration.
- Validation automatisée : `scripts/check-migrations.sh` applique toutes les
  migrations sur un PostgreSQL jetable (conteneur Docker) — sert de test de
  la feature tant que la CI (F10.4) n'est pas en place.

## 5. Références

- [docs/ARCHITECTURE.md §6](../ARCHITECTURE.md#6-modèle-de-données-conceptuel)
- sqlx — <https://github.com/launchbadge/sqlx>
