// Fixture des tests (#257) : extrait de `GET /connectors/actions`, tel que
// le sert l'API (mêmes kinds, clés et types que les schémas Rust).

import type { Schemas } from "./apiClient";
import { BUILTIN_CATALOG, connectorItem, connectorSchema, type Catalog } from "./catalog";

export const SLACK_SEND: Schemas["ActionSchema"] = {
  provider: "slack",
  kind: "slack.send_message",
  label: "Slack — Envoyer un message",
  description: "Publie un message dans un canal Slack via `chat.postMessage`.",
  params: [
    { key: "channel", label: "Canal", kind: { type: "string" }, required: true },
    { key: "text", label: "Texte", kind: { type: "text" }, required: true },
    { key: "thread_ts", label: "Thread (parent ts)", kind: { type: "string" }, required: false },
    {
      key: "blocks",
      label: "Blocs (Block Kit)",
      kind: { type: "array", inner: { type: "object" } },
      required: false,
    },
  ],
};

export const NOTION_CREATE_PAGE: Schemas["ActionSchema"] = {
  provider: "notion",
  kind: "notion.create_page",
  label: "Notion — Créer une page",
  description: "Crée une page via `POST /v1/pages`.",
  params: [
    { key: "parent", label: "Parent", kind: { type: "object" }, required: true },
    { key: "properties", label: "Properties", kind: { type: "object" }, required: true },
  ],
};

export const GITHUB_CLOSE_ISSUE: Schemas["ActionSchema"] = {
  provider: "github",
  kind: "github.close_issue",
  label: "GitHub — Fermer une issue",
  description: "Ferme une issue via `PATCH /repos/{owner}/{repo}/issues/{n}`.",
  params: [
    { key: "owner", label: "Owner", kind: { type: "string" }, required: true },
    { key: "repo", label: "Repo", kind: { type: "string" }, required: true },
    { key: "issue_number", label: "Issue #", kind: { type: "integer" }, required: true },
    {
      key: "state_reason",
      label: "Raison",
      kind: { type: "enum", variants: ["completed", "not_planned"] },
      required: false,
      description: "Motif de fermeture affiché par GitHub.",
    },
  ],
};

export const SAMPLE_ACTIONS = [SLACK_SEND, NOTION_CREATE_PAGE, GITHUB_CLOSE_ISSUE];

/** Catalogue chargé avec les actions de la fixture. */
export const SAMPLE_CATALOG: Catalog = {
  items: [...BUILTIN_CATALOG, ...SAMPLE_ACTIONS.map(connectorItem)],
  schemas: SAMPLE_ACTIONS.map(connectorSchema),
  connectorsUnavailable: false,
};
