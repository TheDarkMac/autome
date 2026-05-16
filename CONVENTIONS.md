# 📏 Conventions & Règles du projet — Autome

> Règles à respecter pour toute contribution. Référence : voir [ROADMAP.md](./ROADMAP.md)
> pour les IDs de features (`Fx.y` / `Fx.y.z`).

---

## 1. Versioning Git (règle imposée)

- **Tout est versionné avec Git.** Aucune partie du projet ne reste hors dépôt.
- **Une branche par feature.** Pas de travail direct sur `main`.
  - Convention de nommage : `feature/F<id>-<slug>`
    - ex. `feature/F1.3-cargo-workspace`, `feature/F2.1-graph-dag`
  - Sous-tâche optionnelle : `feature/F2.7-error-handling` (la branche couvre la feature, on commit par sous-tâche).
  - Autres préfixes : `fix/`, `chore/`, `docs/`, `refactor/`.
- **Commits atomiques** : un commit = un changement cohérent (idéalement une sous-tâche `Fx.y.z`).
- **Pas de mention de Claude / IA comme co-auteur** dans les messages de commit. Aucune ligne `Co-Authored-By` liée à un assistant.
- Branche fusionnée dans `main` une fois la feature **terminée + testée**, puis supprimée.

## 2. Format des messages de commit

Convention **Conventional Commits**, en référant l'ID de feature :

```
<type>(<scope>): <résumé court>  [F<id>]

<corps optionnel : pourquoi, pas seulement quoi>
```

- **types** : `feat`, `fix`, `test`, `refactor`, `docs`, `chore`, `ci`, `perf`
- **exemple** : `feat(engine): tri topologique du DAG [F2.2.a]`
- Impératif présent, ≤ 72 caractères pour le résumé.

## 3. Workflow de contribution

1. Créer la branche depuis `main` à jour.
2. Cocher la sous-tâche `[~]` (en cours) dans `ROADMAP.md`.
3. Développer en **TDD** : test d'abord (rouge) → implémentation (vert) → refactor.
4. `cargo fmt` + `cargo clippy` (zéro warning) avant commit.
5. Commits atomiques au fil de l'eau.
6. Ouvrir une PR vers `main` ; CI verte obligatoire.
7. Merge → cocher `[x]`, mettre à jour le tableau récap + journal de `ROADMAP.md`.

## 4. Qualité de code

- **TDD obligatoire** : pas de code de prod sans test associé (cf. domaine 9 du roadmap).
- Rust : `cargo fmt` (rustfmt), `cargo clippy -- -D warnings`.
- Frontend : ESLint + Prettier, TypeScript en mode strict.
- Pas de secret en clair dans le code/commits (utiliser `.env`, ignoré par Git).
- Code et commentaires cohérents avec le style existant du module.

## 5. Tests & CI

- Une feature n'est `[x]` que si ses tests passent.
- La CI doit : build + `fmt --check` + `clippy` + tests, sur backend et frontend.
- Pas de merge si la CI est rouge.

## 6. Documentation

- Toute feature visible (API, connecteur, node) est documentée à la livraison.
- `README` et guides mis à jour dans la même PR que le code concerné.
- Décisions d'architecture notables → consignées (ADR court dans `/docs`).

## 7. Issues GitHub & labels (règle adoptée)

- **Une issue GitHub par feature** du roadmap (`Fx.y`). Les sous-tâches `Fx.y.z`
  sont des cases à cocher dans le corps de l'issue.
- **Titre** : `[F<id>] <intitulé de la feature>` — ex. `[F2.1] Représentation du graphe de workflow`.
- **Un label par domaine** (couvre le domaine 1→12 du roadmap) :

  | Label | Domaine |
  |---|---|
  | `domain:foundations` | 1. Fondations & Architecture |
  | `domain:engine` | 2. Moteur de workflows |
  | `domain:triggers-actions` | 3. Triggers & Actions |
  | `domain:connectors` | 4. Connecteurs API |
  | `domain:orchestrator` | 5. Orchestrateur & Scaling |
  | `domain:security` | 6. Sécurité & Auth |
  | `domain:api` | 7. API publique |
  | `domain:frontend` | 8. Frontend & Éditeur visuel |
  | `domain:testing` | 9. Tests (TDD) |
  | `domain:devops` | 10. DevOps & Déploiement |
  | `domain:docs` | 11. Documentation |
  | `domain:examples` | 12. Exemples de workflows |

- Labels transverses : `type:feat` / `type:bug` / `type:chore`, `status:blocked`.
- La branche de la feature référence l'issue ; la PR la **ferme** (`Closes #N`).
- Issue fermée ⇒ feature cochée `[x]` dans `ROADMAP.md` (et inversement, cohérence maintenue).

## 8. Définition de « Terminé » (DoD)

Une feature/sous-tâche est terminée quand :
- [ ] Code mergé sur `main`
- [ ] Tests écrits et verts
- [ ] `fmt` + `clippy`/lint OK
- [ ] Documentée si visible
- [ ] Issue GitHub fermée
- [ ] Case cochée + récap/journal `ROADMAP.md` à jour

---

## 💡 Propositions complémentaires (à valider)

- **Protéger `main`** : interdiction de push direct, PR + CI verte requise.
- **`.gitignore` dès le départ** : `/target`, `node_modules`, `.env`, artefacts.
- **Pre-commit hook** : `fmt` + `clippy` + lint front lancés localement avant commit.
- **Semantic versioning + CHANGELOG** : `vMAJOR.MINOR.PATCH`, tag de release sur `main`.
- **PR template** : checklist DoD + lien vers l'ID de feature.
- **Conventional Commits → CHANGELOG auto** (ex. `git-cliff`).

> ✅ Adopté : *une issue GitHub par feature, label par domaine* (voir §7).
>
> Dis-moi lesquelles des restantes tu adoptes : je les intègre comme règles fermes
> (et je peux générer `.gitignore`, hook pre-commit, PR template, config CI).
