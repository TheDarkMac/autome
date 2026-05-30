import { describe, expect, it, vi } from "vitest";
import {
  CATEGORY_ORDER,
  STATIC_CATALOG,
  fetchCatalog,
  groupByCategory,
  searchCatalog,
  type CatalogItem,
} from "./catalog";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("STATIC_CATALOG", () => {
  it("contains items in each category and unique ids", () => {
    const ids = new Set(STATIC_CATALOG.map((it) => it.id));
    expect(ids.size).toBe(STATIC_CATALOG.length);
    for (const cat of CATEGORY_ORDER) {
      expect(STATIC_CATALOG.some((it) => it.category === cat)).toBe(true);
    }
  });

  it("each trigger/action item has a non-empty subtype", () => {
    for (const it of STATIC_CATALOG) {
      if (it.kind === "trigger" || it.kind === "action") {
        expect(it.subtype, `${it.id} doit avoir un subtype`).toBeTruthy();
      }
    }
  });
});

describe("searchCatalog", () => {
  it("returns full list when query is empty or whitespace", () => {
    expect(searchCatalog(STATIC_CATALOG, "")).toHaveLength(STATIC_CATALOG.length);
    expect(searchCatalog(STATIC_CATALOG, "   ")).toHaveLength(
      STATIC_CATALOG.length,
    );
  });

  it("matches on label case-insensitively", () => {
    const hits = searchCatalog(STATIC_CATALOG, "slack");
    expect(hits.length).toBeGreaterThan(0);
    expect(hits.every((it) => it.id.includes("slack"))).toBe(true);
  });

  it("matches on subtype and description", () => {
    expect(searchCatalog(STATIC_CATALOG, "cron")).toHaveLength(1);
    expect(searchCatalog(STATIC_CATALOG, "branchement").length).toBeGreaterThan(
      0,
    );
  });

  it("returns empty when nothing matches", () => {
    expect(searchCatalog(STATIC_CATALOG, "zzz-not-a-node")).toEqual([]);
  });
});

describe("groupByCategory", () => {
  it("respects canonical category order", () => {
    const groups = groupByCategory(STATIC_CATALOG);
    expect(groups.map((g) => g.category)).toEqual([
      "Triggers",
      "Actions",
      "Logique",
    ]);
  });

  it("omits empty categories", () => {
    const subset: CatalogItem[] = STATIC_CATALOG.filter(
      (it) => it.category === "Logique",
    );
    const groups = groupByCategory(subset);
    expect(groups).toHaveLength(1);
    expect(groups[0]?.category).toBe("Logique");
  });
});

describe("fetchCatalog", () => {
  it("returns parsed items when API responds 200 with a valid body", async () => {
    const body = {
      items: [
        {
          id: "trigger.webhook",
          kind: "trigger",
          subtype: "webhook",
          label: "Webhook",
          category: "Triggers",
        },
      ],
    };
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse(body));
    const items = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(items).toHaveLength(1);
    expect(items[0]?.subtype).toBe("webhook");
  });

  it("falls back to STATIC_CATALOG on 404", async () => {
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse({}, 404));
    const items = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(items).toBe(STATIC_CATALOG);
  });

  it("falls back to STATIC_CATALOG when transport throws", async () => {
    const fakeFetch = vi.fn().mockRejectedValue(new Error("ECONNREFUSED"));
    const items = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(items).toBe(STATIC_CATALOG);
  });

  it("falls back when API returns malformed payload (no array)", async () => {
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse({ items: "nope" }));
    const items = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(items).toBe(STATIC_CATALOG);
  });

  it("silently drops malformed entries and keeps valid ones", async () => {
    const body = {
      items: [
        { kind: "trigger", label: "Webhook" }, // manque id + category
        {
          id: "logic.if",
          kind: "if",
          label: "If",
          category: "Logique",
        },
      ],
    };
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse(body));
    const items = await fetchCatalog(fakeFetch as unknown as typeof fetch);
    expect(items).toHaveLength(1);
    expect(items[0]?.id).toBe("logic.if");
  });
});
