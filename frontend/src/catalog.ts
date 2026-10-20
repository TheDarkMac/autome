// Catalogue de nodes (F8.3, #257).
//
// F8.3.a — `CatalogItem` + nodes intégrés + helper de recherche.
// #257   — Les actions de connecteur (Slack, Gmail, Notion, GitHub) ne sont
// plus recopiées ici : `fetchCatalog()` les lit sur `GET /connectors/actions`,
// servies depuis les schémas du SDK Rust — ceux que le worker exécute. La
// copie à la main avait dérivé (`slack.send` au lieu de `slack.send_message`,
// paramètres renommés) et produisait des nodes qui échouaient à l'exécution.
// Chaque action donne un item de palette **et** le schéma de son formulaire.
// Si la route échoue, la palette garde les nodes intégrés seuls et le signale.
//
// Le payload d'un drag-and-drop palette → canvas est sérialisé en JSON
// dans `event.dataTransfer.setData(MIME, ...)`. Le MIME custom évite les
// conflits avec d'autres sources de drop (fichiers, texte).

import { apiBaseUrl } from "./api";
import type { Schemas } from "./apiClient";
import type { WireKind } from "./graph";
import type { NodeSchema, ParamSchema } from "./paramSchema";

/** Catégorie d'affichage dans la palette. */
export type CatalogCategory = "Triggers" | "Actions" | "Logique";

/** Un item du catalogue — projection « palette » d'un type de node. */
export interface CatalogItem {
  /** Identifiant stable pour la liste React + `dataTransfer`. */
  readonly id: string;
  readonly kind: WireKind;
  /** `trigger_type` ou `action_type` selon `kind` (undef pour if/loop/...). */
  readonly subtype?: string;
  /** Libellé affiché. */
  readonly label: string;
  /** Catégorie d'affichage. */
  readonly category: CatalogCategory;
  /** Description courte (1 ligne) — utile en F8.3.a + F8.4. */
  readonly description?: string;
}

/** Catalogue chargé : items de palette + formulaires des actions de connecteur. */
export interface Catalog {
  readonly items: readonly CatalogItem[];
  /** Schémas des actions de connecteur (les intégrés sont dans `NODE_SCHEMAS`). */
  readonly schemas: readonly NodeSchema[];
  /** `GET /connectors/actions` a échoué : seuls les nodes intégrés sont listés. */
  readonly connectorsUnavailable: boolean;
}

/** MIME custom du payload drag-and-drop (F8.3.b). */
export const DND_MIME = "application/x-autome-node";

// ────────────────── Nodes intégrés (F8.3.a) ──────────────────

/**
 * Nodes portés par le moteur lui-même (déclencheurs, `http_request`,
 * logique) : décrits ici, avec leur formulaire dans `paramSchema.ts`.
 */
export const BUILTIN_CATALOG: readonly CatalogItem[] = [
  // Triggers (F3.*)
  {
    id: "trigger.webhook",
    kind: "trigger",
    subtype: "webhook",
    label: "Webhook",
    category: "Triggers",
    description: "Reçoit une requête HTTP entrante.",
  },
  {
    id: "trigger.schedule",
    kind: "trigger",
    subtype: "schedule",
    label: "Schedule (cron)",
    category: "Triggers",
    description: "Déclenche selon une expression cron.",
  },
  {
    id: "trigger.polling",
    kind: "trigger",
    subtype: "polling",
    label: "Polling",
    category: "Triggers",
    description: "Interroge périodiquement un endpoint.",
  },
  // Action intégrée (les actions de connecteur viennent de l'API, #257)
  {
    id: "action.http_request",
    kind: "action",
    subtype: "http_request",
    label: "HTTP Request",
    category: "Actions",
    description: "Appel HTTP générique (GET/POST/…).",
  },
  // Logique (F2.3 / F2.4)
  {
    id: "logic.if",
    kind: "if",
    label: "If",
    category: "Logique",
    description: "Branchement booléen (`true` / `false`).",
  },
  {
    id: "logic.switch",
    kind: "switch",
    label: "Switch",
    category: "Logique",
    description: "Aiguillage multi-branches.",
  },
  {
    id: "logic.loop",
    kind: "loop",
    label: "Loop",
    category: "Logique",
    description: "Itère sur une collection.",
  },
  {
    id: "logic.while",
    kind: "while",
    label: "While",
    category: "Logique",
    description: "Boucle conditionnelle.",
  },
];

/** Catalogue avant chargement : nodes intégrés seuls. */
export const BUILTIN_ONLY: Catalog = {
  items: BUILTIN_CATALOG,
  schemas: [],
  connectorsUnavailable: false,
};

/** Ordre canonique des catégories dans l'UI. */
export const CATEGORY_ORDER: readonly CatalogCategory[] = [
  "Triggers",
  "Actions",
  "Logique",
];

// ──────────────── Recherche (F8.3.a) ────────────────

/** Filtre case-insensitive sur label + subtype + description. */
export function searchCatalog(
  items: readonly CatalogItem[],
  query: string,
): CatalogItem[] {
  const q = query.trim().toLowerCase();
  if (q.length === 0) return [...items];
  return items.filter((it) => {
    const hay = `${it.label} ${it.subtype ?? ""} ${it.description ?? ""}`
      .toLowerCase();
    return hay.includes(q);
  });
}

/** Regroupe une liste d'items par catégorie, dans l'ordre canonique. */
export function groupByCategory(
  items: readonly CatalogItem[],
): Array<{ category: CatalogCategory; items: CatalogItem[] }> {
  const buckets = new Map<CatalogCategory, CatalogItem[]>();
  for (const it of items) {
    const bucket = buckets.get(it.category);
    if (bucket) bucket.push(it);
    else buckets.set(it.category, [it]);
  }
  return CATEGORY_ORDER.filter((c) => buckets.has(c)).map((category) => ({
    category,
    items: buckets.get(category)!,
  }));
}

// ──────────────── Actions de connecteur (#257) ────────────────

type ActionSchema = Schemas["ActionSchema"];
type ParamSpec = Schemas["ParamSpec"];
type ParamKind = Schemas["ParamKind"];

/** Item de palette d'une action de connecteur. */
export function connectorItem(action: ActionSchema): CatalogItem {
  return {
    id: `action.${action.kind}`,
    kind: "action",
    subtype: action.kind,
    label: action.label,
    category: "Actions",
    description: action.description,
  };
}

/** Formulaire d'une action de connecteur, tiré de son schéma. */
export function connectorSchema(action: ActionSchema): NodeSchema {
  return {
    kind: "action",
    subtype: action.kind,
    label: action.label,
    credentialProvider: action.provider,
    params: action.params.map(paramFromSpec),
  };
}

function paramFromSpec(spec: ParamSpec): ParamSchema {
  const base = {
    name: spec.key,
    label: spec.label,
    ...(spec.required ? { required: true } : {}),
  };
  const help = spec.description ?? undefined;
  const kind = spec.kind;
  switch (kind.type) {
    case "string":
      return { ...base, type: "string", ...(help ? { help } : {}) };
    case "text":
      return { ...base, type: "text", ...(help ? { help } : {}) };
    case "integer":
      return { ...base, type: "number", ...(help ? { help } : {}) };
    case "boolean":
      return { ...base, type: "boolean", ...(help ? { help } : {}) };
    case "enum":
      return { ...base, type: "enum", options: kind.variants, ...(help ? { help } : {}) };
    case "object":
    case "array":
      return {
        ...base,
        type: "json",
        jsonShape: kind.type,
        placeholder: jsonExample(kind),
        help: help ?? `JSON (${kind.type === "array" ? "tableau" : "objet"}) ou expression {{ … }}.`,
      };
  }
}

/** Exemple de valeur pour le placeholder d'un champ JSON. */
function jsonExample(kind: ParamKind): string {
  switch (kind.type) {
    case "array":
      return `[${jsonExample(kind.inner)}]`;
    case "object":
      return '{"clé": "valeur"}';
    case "integer":
      return "1";
    case "boolean":
      return "true";
    case "enum":
      return JSON.stringify(kind.variants[0] ?? "");
    default:
      return '"…"';
  }
}

// ──────────────── Chargement (#257) ────────────────

/**
 * Charge les actions de connecteur depuis l'API et les ajoute aux nodes
 * intégrés. Ne lève jamais : sur 404, erreur de transport ou réponse mal
 * formée, renvoie les nodes intégrés seuls avec `connectorsUnavailable`.
 */
export async function fetchCatalog(
  fetchImpl: typeof fetch = fetch,
): Promise<Catalog> {
  const unavailable: Catalog = { ...BUILTIN_ONLY, connectorsUnavailable: true };
  try {
    const res = await fetchImpl(`${apiBaseUrl()}/connectors/actions`, {
      headers: { Accept: "application/json" },
    });
    if (!res.ok) return unavailable;
    const body = (await res.json()) as { actions?: unknown };
    if (!Array.isArray(body.actions)) return unavailable;
    const actions = body.actions.filter(isActionSchema);
    return {
      items: [...BUILTIN_CATALOG, ...actions.map(connectorItem)],
      schemas: actions.map(connectorSchema),
      connectorsUnavailable: false,
    };
  } catch {
    return unavailable;
  }
}

const PARAM_TYPES = new Set(["string", "text", "integer", "boolean", "object", "array", "enum"]);

/** Écarte une entrée mal formée plutôt que de casser toute la palette. */
function isActionSchema(value: unknown): value is ActionSchema {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v["kind"] === "string" &&
    typeof v["label"] === "string" &&
    typeof v["provider"] === "string" &&
    typeof v["description"] === "string" &&
    Array.isArray(v["params"]) &&
    v["params"].every(isParamSpec)
  );
}

function isParamSpec(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v["key"] === "string" &&
    typeof v["label"] === "string" &&
    typeof v["required"] === "boolean" &&
    isParamKind(v["kind"])
  );
}

function isParamKind(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  if (typeof v["type"] !== "string" || !PARAM_TYPES.has(v["type"])) return false;
  if (v["type"] === "array") return isParamKind(v["inner"]);
  if (v["type"] === "enum") {
    return Array.isArray(v["variants"]) && v["variants"].every((x) => typeof x === "string");
  }
  return true;
}
