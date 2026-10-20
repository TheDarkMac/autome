import { describe, expect, it, vi } from "vitest";
import {
  BUILTIN_CATALOG,
  BUILTIN_ONLY,
  CATEGORY_ORDER,
  connectorItem,
  connectorSchema,
  fetchCatalog,
  groupByCategory,
  searchCatalog,
  type CatalogItem,
} from "./catalog";
import { GITHUB_CLOSE_ISSUE, SAMPLE_ACTIONS, SAMPLE_CATALOG, SLACK_SEND } from "./testCatalog";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const CATALOG = SAMPLE_CATALOG.items;

describe("BUILTIN_CATALOG", () => {
  it("contains items in each category and unique ids", () => {
    const ids = new Set(BUILTIN_CATALOG.map((it) => it.id));
    expect(ids.size).toBe(BUILTIN_CATALOG.length);
    for (const cat of CATEGORY_ORDER) {
      expect(BUILTIN_CATALOG.some((it) => it.category === cat)).toBe(true);
    }
  });

  it("ne recopie plus aucune action de connecteur (#257)", () => {
    const actions = BUILTIN_CATALOG.filter((it) => it.kind === "action");
    expect(actions.map((it) => it.subtype)).toEqual(["http_request"]);
  });

  it("each trigger/action item has a non-empty subtype", () => {
    for (const it of BUILTIN_CATALOG) {
      if (it.kind === "trigger" || it.kind === "action") {
        expect(it.subtype, `${it.id} doit avoir un subtype`).toBeTruthy();
      }
    }
  });
});

describe("searchCatalog", () => {
  it("returns full list when query is empty or whitespace", () => {
    expect(searchCatalog(CATALOG, "")).toHaveLength(CATALOG.length);
    expect(searchCatalog(CATALOG, "   ")).toHaveLength(
      CATALOG.length,
    );
  });

  it("matches on label case-insensitively", () => {
    const hits = searchCatalog(CATALOG, "slack");
    expect(hits.length).toBeGreaterThan(0);
    expect(hits.every((it) => it.id.includes("slack"))).toBe(true);
  });

  it("matches on subtype and description", () => {
    expect(searchCatalog(CATALOG, "cron")).toHaveLength(1);
    expect(searchCatalog(CATALOG, "branchement").length).toBeGreaterThan(
      0,
    );
  });

  it("returns empty when nothing matches", () => {
    expect(searchCatalog(CATALOG, "zzz-not-a-node")).toEqual([]);
  });
});

describe("groupByCategory", () => {
  it("respects canonical category order", () => {
    const groups = groupByCategory(CATALOG);
    expect(groups.map((g) => g.category)).toEqual([
      "Triggers",
      "Actions",
      "Logique",
    ]);
  });

  it("omits empty categories", () => {
    const subset: CatalogItem[] = CATALOG.filter(
      (it) => it.category === "Logique",
    );
    const groups = groupByCategory(subset);
    expect(groups).toHaveLength(1);
    expect(groups[0]?.category).toBe("Logique");
  });
});

describe("actions de connecteur (#257)", () => {
  it("donne un item de palette par action, identifié par son kind", () => {
    expect(connectorItem(SLACK_SEND)).toEqual({
      id: "action.slack.send_message",
      kind: "action",
      subtype: "slack.send_message",
      label: "Slack — Envoyer un message",
      category: "Actions",
      description: "Publie un message dans un canal Slack via `chat.postMessage`.",
    });
  });

  it("traduit les paramètres du schéma en champs de formulaire", () => {
    const schema = connectorSchema(SLACK_SEND);
    expect(schema).toMatchObject({
      kind: "action",
      subtype: "slack.send_message",
      credentialProvider: "slack",
    });
    expect(schema.params).toEqual([
      { name: "channel", label: "Canal", required: true, type: "string" },
      { name: "text", label: "Texte", required: true, type: "text" },
      { name: "thread_ts", label: "Thread (parent ts)", type: "string" },
      {
        name: "blocks",
        label: "Blocs (Block Kit)",
        type: "json",
        jsonShape: "array",
        placeholder: '[{"clé": "valeur"}]',
        help: "JSON (tableau) ou expression {{ … }}.",
      },
    ]);
  });

  it("entier → nombre, énumération → options, description → aide", () => {
    const params = connectorSchema(GITHUB_CLOSE_ISSUE).params;
    expect(params.find((p) => p.name === "issue_number")).toMatchObject({
      type: "number",
      required: true,
    });
    expect(params.find((p) => p.name === "state_reason")).toEqual({
      name: "state_reason",
      label: "Raison",
      type: "enum",
      options: ["completed", "not_planned"],
      help: "Motif de fermeture affiché par GitHub.",
    });
  });

  it("champ JSON : objet, booléen et tableaux imbriqués", () => {
    const schema = connectorSchema({
      ...SLACK_SEND,
      params: [
        { key: "o", label: "O", kind: { type: "object" }, required: true },
        { key: "b", label: "B", kind: { type: "boolean" }, required: false },
        {
          key: "l",
          label: "L",
          kind: { type: "array", inner: { type: "array", inner: { type: "integer" } } },
          required: false,
          description: "Liste de listes.",
        },
        {
          key: "e",
          label: "E",
          kind: { type: "array", inner: { type: "enum", variants: ["a"] } },
          required: false,
        },
        {
          key: "f",
          label: "F",
          kind: { type: "array", inner: { type: "boolean" } },
          required: false,
        },
        {
          key: "s",
          label: "S",
          kind: { type: "array", inner: { type: "string" } },
          required: false,
        },
      ],
    });
    const [o, b, l, e, f, s] = schema.params;
    expect(o).toMatchObject({ type: "json", jsonShape: "object", placeholder: '{"clé": "valeur"}' });
    expect(b).toMatchObject({ type: "boolean" });
    expect(l).toMatchObject({ placeholder: "[[1]]", help: "Liste de listes." });
    expect(e?.placeholder).toBe('["a"]');
    expect(f?.placeholder).toBe("[true]");
    expect(s?.placeholder).toBe('["…"]');
  });
});

describe("fetchCatalog", () => {
  it("ajoute aux nodes intégrés les actions servies par l'API", async () => {
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse({ actions: SAMPLE_ACTIONS }));
    const catalog = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(fakeFetch.mock.calls[0]?.[0]).toMatch(/\/connectors\/actions$/);
    expect(catalog.connectorsUnavailable).toBe(false);
    expect(catalog.items).toEqual(SAMPLE_CATALOG.items);
    expect(catalog.schemas.map((s) => s.subtype)).toEqual([
      "slack.send_message",
      "notion.create_page",
      "github.close_issue",
    ]);
  });

  it.each([
    ["404", () => Promise.resolve(jsonResponse({}, 404))],
    ["erreur de transport", () => Promise.reject(new Error("ECONNREFUSED"))],
    ["réponse sans tableau", () => Promise.resolve(jsonResponse({ actions: "nope" }))],
  ])("garde les nodes intégrés seuls et le signale (%s)", async (_label, impl) => {
    const fakeFetch = vi.fn().mockImplementation(impl);
    const catalog = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(catalog).toEqual({ ...BUILTIN_ONLY, connectorsUnavailable: true });
  });

  it("écarte les entrées mal formées et garde les autres", async () => {
    const body = {
      actions: [
        null,
        { kind: "x.y", label: "X" }, // manque provider, description, params
        { ...SLACK_SEND, params: [{ key: "k", label: "K", required: true }] }, // kind absent
        { ...SLACK_SEND, params: [{ key: "k", label: "K", required: true, kind: { type: "date" } }] },
        { ...SLACK_SEND, params: [{ key: "k", label: "K", required: true, kind: { type: "enum" } }] },
        { ...SLACK_SEND, params: [{ key: "k", label: "K", required: true, kind: { type: "array" } }] },
        { ...SLACK_SEND, params: [null] },
        { ...SLACK_SEND, params: [{ key: "k", label: "K", required: true, kind: null }] },
        GITHUB_CLOSE_ISSUE,
      ],
    };
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse(body));
    const catalog = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(catalog.schemas.map((s) => s.subtype)).toEqual(["github.close_issue"]);
    expect(catalog.items.at(-1)?.id).toBe("action.github.close_issue");
  });
});
