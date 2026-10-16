// Schémas de paramètres par node (F8.4.a/b/c/d).
//
// Chaque node du catalogue (cf. `catalog.ts`) est associé à un `NodeSchema`
// qui décrit ses champs. Le `NodePanel` rend le formulaire dynamiquement
// depuis ce schéma — pas de code spécifique par type côté UI.
//
// Le schéma porte aussi :
//   - les valeurs par défaut (F8.4.b — appliquées à l'ajout d'un node)
//   - les contraintes de validation (`required`, `pattern`, `min`, `max`)
//   - un éventuel `credentialProvider` (F8.4.d) pour brancher la dropdown
//
// L'éditeur d'expressions (F8.4.c) ne modifie pas le schéma — il se
// contente de détecter `{{ $node('id').path }}` dans les valeurs string
// et d'en extraire la liste pour affichage. Aucune évaluation côté
// frontend (c'est le backend qui interprète).

import type { WireKind } from "./graph";

// ─────────────────────────── types ───────────────────────────

export type ParamType = "string" | "text" | "number" | "boolean" | "enum";

export interface ParamSchema {
  readonly name: string;
  readonly label: string;
  readonly type: ParamType;
  readonly required?: boolean;
  readonly default?: ParamValue;
  /** Pour `type: "enum"`. */
  readonly options?: readonly string[];
  readonly placeholder?: string;
  readonly help?: string;
  /** Pattern regex (string source) pour `type: "string"` ou `"text"`. */
  readonly pattern?: string;
  /** Bornes pour `type: "number"`. */
  readonly min?: number;
  readonly max?: number;
}

export type ParamValue = string | number | boolean | null;
export type ParamValues = Record<string, ParamValue>;

export interface NodeSchema {
  readonly kind: WireKind;
  readonly subtype?: string;
  readonly label: string;
  readonly params: readonly ParamSchema[];
  /** Nom du provider attendu pour la sélection de credentials (F8.4.d). */
  readonly credentialProvider?: string;
}

// ─────────────────────────── registre ───────────────────────────

/** Registre statique : un schéma par couple (kind, subtype?). */
export const NODE_SCHEMAS: readonly NodeSchema[] = [
  // Triggers
  {
    kind: "trigger",
    subtype: "webhook",
    label: "Webhook",
    params: [
      {
        name: "path",
        label: "Libellé",
        type: "string",
        placeholder: "/hooks/mon-event",
        pattern: "^/.*",
        help: "Repère libre : l'URL réelle, secrète, est générée par le serveur.",
      },
      {
        name: "method",
        label: "Méthode",
        type: "enum",
        options: ["GET", "POST", "PUT", "DELETE"],
        default: "POST",
      },
      {
        name: "signature",
        label: "Signature",
        type: "enum",
        options: ["none", "github"],
        help: "github : exige X-Hub-Signature-256 (secret de signature ci-dessous).",
      },
      {
        name: "event",
        label: "Événement GitHub",
        type: "string",
        placeholder: "issues",
        help: "Si renseigné, les autres valeurs de X-GitHub-Event sont ignorées.",
      },
    ],
  },
  {
    kind: "trigger",
    subtype: "schedule",
    label: "Schedule (cron)",
    params: [
      {
        name: "cron",
        label: "Expression cron",
        type: "string",
        required: true,
        placeholder: "*/5 * * * *",
        help: "Format standard 5 champs.",
      },
      {
        name: "timezone",
        label: "Timezone",
        type: "string",
        default: "UTC",
      },
    ],
  },
  {
    kind: "trigger",
    subtype: "polling",
    label: "Polling",
    params: [
      { name: "url", label: "URL", type: "string", required: true },
      {
        name: "interval_seconds",
        label: "Intervalle (s)",
        type: "number",
        default: 60,
        min: 5,
      },
    ],
  },
  // Actions
  {
    kind: "action",
    subtype: "http_request",
    label: "HTTP Request",
    params: [
      { name: "url", label: "URL", type: "string", required: true },
      {
        name: "method",
        label: "Méthode",
        type: "enum",
        options: ["GET", "POST", "PUT", "PATCH", "DELETE"],
        default: "GET",
      },
      { name: "body", label: "Body", type: "text" },
    ],
  },
  {
    kind: "action",
    subtype: "slack.send",
    label: "Slack — Send Message",
    credentialProvider: "slack",
    params: [
      { name: "channel", label: "Channel", type: "string", required: true, placeholder: "#general" },
      { name: "message", label: "Message", type: "text", required: true },
    ],
  },
  {
    kind: "action",
    subtype: "gmail.send",
    label: "Gmail — Send Email",
    credentialProvider: "gmail",
    params: [
      { name: "to", label: "Destinataire", type: "string", required: true },
      { name: "subject", label: "Sujet", type: "string", required: true },
      { name: "body", label: "Corps", type: "text", required: true },
    ],
  },
  {
    kind: "action",
    subtype: "notion.create_page",
    label: "Notion — Create Page",
    credentialProvider: "notion",
    params: [
      { name: "database_id", label: "Database ID", type: "string", required: true },
      { name: "title", label: "Titre", type: "string", required: true },
    ],
  },
  {
    kind: "action",
    subtype: "github.create_issue",
    label: "GitHub — Create Issue",
    credentialProvider: "github",
    params: [
      { name: "repo", label: "Repo (owner/name)", type: "string", required: true, pattern: "^[^/]+/[^/]+$" },
      { name: "title", label: "Titre", type: "string", required: true },
      { name: "body", label: "Description", type: "text" },
    ],
  },
  // Logique
  {
    kind: "if",
    label: "If",
    params: [
      {
        name: "condition",
        label: "Condition",
        type: "string",
        required: true,
        help: "Expression évaluée — supporte le mapping {{ $node('id').field }}.",
      },
    ],
  },
  {
    kind: "switch",
    label: "Switch",
    params: [
      { name: "expression", label: "Expression", type: "string", required: true },
    ],
  },
  {
    kind: "loop",
    label: "Loop",
    params: [
      {
        name: "collection",
        label: "Collection",
        type: "string",
        required: true,
        help: "Référence à un tableau, ex. {{ $node('http-1').output.items }}.",
      },
    ],
  },
  {
    kind: "while",
    label: "While",
    params: [
      { name: "condition", label: "Condition", type: "string", required: true },
      { name: "max_iterations", label: "Max iterations", type: "number", default: 100, min: 1 },
    ],
  },
];

/** Cherche le schéma associé à un node. */
export function findNodeSchema(
  kind: WireKind,
  subtype?: string,
): NodeSchema | undefined {
  return NODE_SCHEMAS.find(
    (s) => s.kind === kind && (s.subtype ?? undefined) === (subtype ?? undefined),
  );
}

// ─────────────────────── defaults (F8.4.b) ───────────────────────

/** Construit l'objet params initial d'un node depuis son schéma. */
export function defaultParams(schema: NodeSchema): ParamValues {
  const out: ParamValues = {};
  for (const p of schema.params) {
    if (p.default !== undefined) out[p.name] = p.default;
  }
  return out;
}

// ─────────────────────── validation (F8.4.b) ───────────────────────

export type ValidationErrors = Record<string, string>;

const EMPTY: ValidationErrors = Object.freeze({});

/** Retourne `{}` si tout est valide, sinon `{ champ: message }`. */
export function validateParams(
  schema: NodeSchema,
  values: ParamValues,
): ValidationErrors {
  const errors: ValidationErrors = {};
  for (const p of schema.params) {
    const v = values[p.name];
    const empty =
      v === undefined || v === null || (typeof v === "string" && v.length === 0);
    if (p.required && empty) {
      errors[p.name] = "Champ requis.";
      continue;
    }
    if (empty) continue;
    switch (p.type) {
      case "string":
      case "text": {
        if (typeof v !== "string") {
          errors[p.name] = "Doit être un texte.";
          break;
        }
        if (p.pattern && !new RegExp(p.pattern).test(v)) {
          errors[p.name] = "Format invalide.";
        }
        break;
      }
      case "number": {
        const n = typeof v === "number" ? v : Number(v);
        if (!Number.isFinite(n)) {
          errors[p.name] = "Doit être un nombre.";
          break;
        }
        if (p.min !== undefined && n < p.min) {
          errors[p.name] = `Doit être ≥ ${p.min}.`;
          break;
        }
        if (p.max !== undefined && n > p.max) {
          errors[p.name] = `Doit être ≤ ${p.max}.`;
        }
        break;
      }
      case "boolean":
        if (typeof v !== "boolean") errors[p.name] = "Doit être un booléen.";
        break;
      case "enum":
        if (typeof v !== "string" || !p.options?.includes(v)) {
          errors[p.name] = "Valeur non autorisée.";
        }
        break;
    }
  }
  return Object.keys(errors).length === 0 ? EMPTY : errors;
}

// ─────────────────────── expressions (F8.4.c) ───────────────────────

export interface ExpressionReference {
  /** Forme brute matchée, ex. `{{ $node('http-1').output.items }}`. */
  readonly raw: string;
  /** Identifiant du node référencé. */
  readonly nodeId: string;
  /** Chemin optionnel après le `.` (ex. `output.items`). */
  readonly path?: string;
}

const REF_RE =
  /\{\{\s*\$node\(\s*['"]([^'"]+)['"]\s*\)(?:\.([\w.[\]]+))?\s*\}\}/g;

/** Extrait toutes les références `{{ $node('id').path }}` d'une chaîne. */
export function extractReferences(value: unknown): ExpressionReference[] {
  if (typeof value !== "string" || value.length === 0) return [];
  const out: ExpressionReference[] = [];
  REF_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = REF_RE.exec(value)) !== null) {
    const nodeId = m[1] ?? "";
    const path = m[2];
    out.push({
      raw: m[0],
      nodeId,
      ...(path !== undefined ? { path } : {}),
    });
  }
  return out;
}

/** Coerce une string de champ number vers `number | null` (pour onChange). */
export function coerceNumberInput(raw: string): number | null {
  if (raw.trim().length === 0) return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}
