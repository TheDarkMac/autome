# 🧪 Exemples de workflows

Workflows automatisés de démonstration (domaine 12 de la [ROADMAP](../ROADMAP.md)).
Chaque exemple est livré comme un **graphe `WorkflowGraph` désérialisable**
(`workflows/*.json`) **et** un **test d'intégration exécutable** qui le fait
tourner de bout en bout via le vrai moteur et les vrais connecteurs, sur un
transport HTTP factice (aucun réseau).

| Exemple | Graphe | Test | Feature |
|---|---|---|---|
| **Slack → Gmail → Notion** — lit le dernier message d'un canal Slack, en envoie un résumé par email, puis l'archive dans Notion | [`workflows/slack-to-gmail-notion.json`](workflows/slack-to-gmail-notion.json) | [`crates/engine/tests/example_slack_gmail_notion.rs`](../crates/engine/tests/example_slack_gmail_notion.rs) | F12.1 |
| **Webhook GitHub → Slack** — authentifie un webhook GitHub (`issues/opened`) puis notifie un canal Slack | [`workflows/github-webhook-to-slack.json`](workflows/github-webhook-to-slack.json) | [`crates/engine/tests/example_github_webhook_to_slack.rs`](../crates/engine/tests/example_github_webhook_to_slack.rs) | F12.2 |

Lancer les exemples :

```bash
cargo test -p autome-engine --test example_slack_gmail_notion
cargo test -p autome-engine --test example_github_webhook_to_slack
```

## Anatomie d'un exemple

1. **Le graphe** (`workflows/*.json`) : un `trigger` puis une chaîne d'`action`
   nodes (`action_type` = `slack.read_channel`, `gmail.send_email`, …) reliés par
   des `edges`. C'est l'artefact qu'un éditeur visuel produirait.
2. **Le test** charge le JSON en `WorkflowGraph`, construit le `Dag`, puis appelle
   `Engine::run` avec un `NodeRunner` qui :
   - instancie le **connecteur réel** correspondant à chaque `action_type` ;
   - **mappe les données** entre nodes (la sortie de Slack alimente le corps de
     l'email, etc.) en lisant `RunContext::output` — ce que le mapping (F2.6)
     automatise en production ;
   - lui injecte un transport factice (`HttpClient` `Sync`) qui enregistre les
     requêtes et rejoue des réponses scriptées.
3. **Les assertions** vérifient le statut du run **et** que chaque service a reçu
   la bonne requête (URL, méthode, corps), prouvant la circulation des données.

> ℹ️ Voir aussi le [guide connecteur](../docs/CONNECTORS.md) pour le détail des
> actions et de leurs paramètres.
