// Catalogue de nodes (F8.3).
//
// F8.3.a — `CatalogItem` + catalogue statique + helper de recherche.
// F8.3.c — `fetchCatalog()` interroge `GET /catalog/nodes` du backend ; si
// l'endpoint n'existe pas encore (404) ou si le transport échoue, on
// retombe **silencieusement** sur le catalogue statique embed (le frontend
// reste fonctionnel même si le backend ne sert pas encore le catalogue).
//
// Le payload d'un drag-and-drop palette → canvas est sérialisé en JSON
// dans `event.dataTransfer.setData(MIME, ...)`. Le MIME custom évite les
// conflits avec d'autres sources de drop (fichiers, texte).

import { apiBaseUrl } from "./api";
import type { WireKind } from "./graph";

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

/** MIME custom du payload drag-and-drop (F8.3.b). */
export const DND_MIME = "application/x-autome-node";

// ────────────────── Catalogue statique embed (F8.3.a) ──────────────────

/** Source de vérité côté frontend tant que le backend ne sert pas /catalog/nodes. */
export const STATIC_CATALOG: readonly CatalogItem[] = [
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
  // Actions (F4.*)
  {
    id: "action.http_request",
    kind: "action",
    subtype: "http_request",
    label: "HTTP Request",
    category: "Actions",
    description: "Appel HTTP générique (GET/POST/…).",
  },
  {
    id: "action.slack.send",
    kind: "action",
    subtype: "slack.send",
    label: "Slack — Send Message",
    category: "Actions",
    description: "Poste un message dans un channel Slack.",
  },
  {
    id: "action.gmail.send",
    kind: "action",
    subtype: "gmail.send",
    label: "Gmail — Send Email",
    category: "Actions",
    description: "Envoie un email via Gmail.",
  },
  {
    id: "action.notion.create_page",
    kind: "action",
    subtype: "notion.create_page",
    label: "Notion — Create Page",
    category: "Actions",
    description: "Crée une page dans Notion.",
  },
  {
    id: "action.github.create_issue",
    kind: "action",
    subtype: "github.create_issue",
    label: "GitHub — Create Issue",
    category: "Actions",
    description: "Ouvre une issue GitHub.",
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

// ──────────────── Chargement dynamique (F8.3.c) ────────────────

interface CatalogResponse {
  items?: unknown;
}

/**
 * Récupère le catalogue depuis l'API ; retombe sur `STATIC_CATALOG` si
 * l'endpoint répond 404, si le transport échoue, ou si la réponse est
 * mal formée. Aucun jet d'erreur — la palette doit rester fonctionnelle.
 */
export async function fetchCatalog(
  fetchImpl: typeof fetch = fetch,
): Promise<readonly CatalogItem[]> {
  try {
    const res = await fetchImpl(`${apiBaseUrl()}/catalog/nodes`, {
      headers: { Accept: "application/json" },
    });
    if (!res.ok) return STATIC_CATALOG;
    const body = (await res.json()) as CatalogResponse;
    const parsed = parseCatalogItems(body.items);
    return parsed.length > 0 ? parsed : STATIC_CATALOG;
  } catch {
    return STATIC_CATALOG;
  }
}

function parseCatalogItems(raw: unknown): CatalogItem[] {
  if (!Array.isArray(raw)) return [];
  const out: CatalogItem[] = [];
  for (const entry of raw) {
    const item = coerceCatalogItem(entry);
    if (item) out.push(item);
  }
  return out;
}

function coerceCatalogItem(value: unknown): CatalogItem | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const id = typeof v["id"] === "string" ? v["id"] : null;
  const kind = isWireKind(v["kind"]) ? v["kind"] : null;
  const label = typeof v["label"] === "string" ? v["label"] : null;
  const category = isCategory(v["category"]) ? v["category"] : null;
  if (!id || !kind || !label || !category) return null;
  const subtype = typeof v["subtype"] === "string" ? v["subtype"] : undefined;
  const description =
    typeof v["description"] === "string" ? v["description"] : undefined;
  const item: CatalogItem = { id, kind, label, category };
  return {
    ...item,
    ...(subtype !== undefined ? { subtype } : {}),
    ...(description !== undefined ? { description } : {}),
  };
}

function isWireKind(v: unknown): v is WireKind {
  return (
    v === "trigger" ||
    v === "action" ||
    v === "if" ||
    v === "switch" ||
    v === "loop" ||
    v === "while"
  );
}

function isCategory(v: unknown): v is CatalogCategory {
  return v === "Triggers" || v === "Actions" || v === "Logique";
}
