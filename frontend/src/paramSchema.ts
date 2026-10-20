// Schémas de paramètres par node (F8.4.a/b/c/d).
//
// Chaque node du catalogue (cf. `catalog.ts`) est associé à un `NodeSchema`
// qui décrit ses champs : `NODE_SCHEMAS` pour les nodes intégrés, et, pour
// les actions de connecteur, les schémas tirés de `GET /connectors/actions`
// (#257, passés à `findNodeSchema`). Le `NodePanel` rend le formulaire dynamiquement
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

export type ParamType = "string" | "text" | "number" | "boolean" | "enum" | "json";

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
  /** Bornes pour `type: "number"` (appliquées aux seuls nombres). */
  readonly min?: number;
  readonly max?: number;
  /**
   * Pour `type: "number"` : accepte aussi une expression `{{ … }}` occupant
   * toute la valeur, que le moteur remplace en gardant le type de la valeur
   * référencée (#261). Réservé aux actions de connecteur : les nombres des
   * nodes intégrés ne passent pas par ce templating.
   */
  readonly expression?: boolean;
  /** Pour `type: "json"` : forme attendue (#257). */
  readonly jsonShape?: "object" | "array";
}

/** Objet ou tableau JSON, valeur d'un champ `type: "json"` (#257). */
export type JsonCollection = readonly unknown[] | { readonly [key: string]: unknown };

export type ParamValue = string | number | boolean | null | JsonCollection;
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
        placeholder: "*/5 * * * *",
        help: "Format standard 5 champs (plages 1-5, pas */15, listes). Ou un intervalle ci-dessous.",
      },
      {
        name: "interval_seconds",
        label: "Ou intervalle (s)",
        type: "number",
        min: 5,
        max: 2_678_400,
        help: "Exclusif avec le cron : renseigner l'un ou l'autre.",
      },
      {
        name: "timezone",
        label: "Timezone",
        type: "enum",
        options: ["UTC"],
        default: "UTC",
        help: "Les expressions cron sont évaluées en UTC.",
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
        max: 2_678_400,
      },
      {
        name: "items_path",
        label: "Chemin du tableau",
        type: "string",
        placeholder: "data.items",
        help: "Vide : la réponse doit être un tableau JSON.",
      },
      {
        name: "key",
        label: "Champ identifiant",
        type: "string",
        default: "id",
        help: "Un run par élément dont ce champ n'a pas encore été vu. Le premier poll sert de référence.",
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

/**
 * Cherche le schéma associé à un node : nodes intégrés, puis `extra` (les
 * actions de connecteur chargées depuis l'API, #257).
 */
export function findNodeSchema(
  kind: WireKind,
  subtype?: string,
  extra: readonly NodeSchema[] = [],
): NodeSchema | undefined {
  const matches = (s: NodeSchema): boolean =>
    s.kind === kind && (s.subtype ?? undefined) === (subtype ?? undefined);
  return NODE_SCHEMAS.find(matches) ?? extra.find(matches);
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
        if (p.expression && typeof v === "string") {
          if (!isSoleExpression(v)) errors[p.name] = "Nombre ou expression {{ … }} attendu.";
          break;
        }
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
      case "json": {
        const error = jsonError(v, p.jsonShape);
        if (error) errors[p.name] = error;
        break;
      }
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

/**
 * `true` si `s` est une seule expression `{{ … }}` : le moteur la remplace
 * alors par la valeur référencée, type compris (`sole_expression`,
 * `crates/engine/src/mapping.rs`). Mêlée à du texte, elle donnerait une
 * chaîne, refusée par un paramètre entier.
 */
export function isSoleExpression(s: string): boolean {
  const t = s.trim();
  if (!t.startsWith("{{") || !t.endsWith("}}") || t.length < 4) return false;
  const inner = t.slice(2, -2);
  return inner.trim().length > 0 && !inner.includes("{{") && !inner.includes("}}");
}

/**
 * Valeur d'un champ nombre qui accepte une expression (#261) : un nombre si
 * la saisie en est un, sinon le texte tel quel (expression, ou saisie à
 * corriger que la validation signale).
 */
export function parseNumberOrExpression(raw: string): ParamValue {
  const t = raw.trim();
  if (t.length === 0) return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : raw;
}

/** Coerce une string de champ number vers `number | null` (pour onChange). */
export function coerceNumberInput(raw: string): number | null {
  if (raw.trim().length === 0) return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

// ─────────────────────── champs JSON (#257) ───────────────────────

/**
 * Valeur d'un champ JSON saisie en texte : objet ou tableau si le texte en
 * est un, sinon le texte tel quel (une expression `{{ … }}`, que le moteur
 * remplace par la valeur référencée, ou une saisie à corriger).
 */
export function parseJsonInput(raw: string): ParamValue {
  if (raw.trim().length === 0) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed === "object" && parsed !== null) return parsed as JsonCollection;
  } catch {
    // texte libre : expression ou saisie en cours
  }
  return raw;
}

/** Texte affiché dans un champ JSON pour une valeur stockée. */
export function jsonDraft(value: ParamValue | undefined): string {
  if (typeof value === "string") return value;
  if (typeof value === "object" && value !== null) return JSON.stringify(value, null, 2);
  return "";
}

function jsonError(v: ParamValue, shape: "object" | "array" | undefined): string | null {
  if (typeof v === "string") return v.includes("{{") ? null : "JSON invalide.";
  if (Array.isArray(v)) return shape === "object" ? "Objet JSON attendu." : null;
  if (typeof v === "object" && v !== null) {
    return shape === "array" ? "Tableau JSON attendu." : null;
  }
  return "JSON invalide.";
}
