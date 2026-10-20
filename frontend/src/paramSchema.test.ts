import { describe, expect, it } from "vitest";

import {
  NODE_SCHEMAS,
  coerceNumberInput,
  defaultParams,
  extractReferences,
  findNodeSchema,
  isSoleExpression,
  jsonDraft,
  parseJsonInput,
  parseNumberOrExpression,
  validateParams,
} from "./paramSchema";
import { SAMPLE_CATALOG } from "./testCatalog";

const CONNECTORS = SAMPLE_CATALOG.schemas;

describe("paramSchema — registre statique", () => {
  it("expose un schéma par couple (kind, subtype) sans doublon", () => {
    const keys = NODE_SCHEMAS.map((s) => `${s.kind}:${s.subtype ?? ""}`);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("trouve le schéma trigger/webhook", () => {
    const s = findNodeSchema("trigger", "webhook");
    expect(s?.label).toBe("Webhook");
    expect(s?.params.map((p) => p.name)).toEqual(["path", "method", "signature", "event"]);
    // #202 : l'URL réelle est générée par le serveur, `path` n'est qu'un libellé.
    expect(s?.params.find((p) => p.name === "path")?.required).toBeUndefined();
  });

  it("trouve le schéma if (sans subtype)", () => {
    const s = findNodeSchema("if");
    expect(s).toBeDefined();
    expect(s?.params[0]?.name).toBe("condition");
  });

  it("retourne undefined pour un couple inconnu", () => {
    expect(findNodeSchema("action", "inexistant.subtype")).toBeUndefined();
  });

  it("cherche les actions de connecteur dans les schémas chargés (#257)", () => {
    expect(findNodeSchema("action", "slack.send_message")).toBeUndefined();
    expect(findNodeSchema("action", "slack.send_message", CONNECTORS)?.credentialProvider).toBe(
      "slack",
    );
    expect(findNodeSchema("action", "github.close_issue", CONNECTORS)?.credentialProvider).toBe(
      "github",
    );
    // Les nodes intégrés restent prioritaires.
    expect(findNodeSchema("action", "http_request", CONNECTORS)?.label).toBe("HTTP Request");
  });

  it("ne déclare pas de credentialProvider pour http_request", () => {
    expect(findNodeSchema("action", "http_request")?.credentialProvider).toBeUndefined();
  });
});

describe("defaultParams", () => {
  it("renvoie uniquement les params ayant un default", () => {
    const s = findNodeSchema("trigger", "webhook")!;
    expect(defaultParams(s)).toEqual({ method: "POST" });
  });

  it("renvoie un objet vide si aucun default", () => {
    const s = findNodeSchema("if")!;
    expect(defaultParams(s)).toEqual({});
  });

  it("préserve les types number et string", () => {
    const s = findNodeSchema("while")!;
    expect(defaultParams(s)).toEqual({ max_iterations: 100 });
  });
});

describe("validateParams", () => {
  const webhook = findNodeSchema("trigger", "webhook")!;
  const polling = findNodeSchema("trigger", "polling")!;
  const slack = findNodeSchema("action", "slack.send_message", CONNECTORS)!;
  const notion = findNodeSchema("action", "notion.create_page", CONNECTORS)!;

  it("flag les champs required manquants", () => {
    const errs = validateParams(polling, {});
    expect(errs).toMatchObject({ url: "Champ requis." });
  });

  it("rejette une string ne matchant pas le pattern", () => {
    const errs = validateParams(webhook, { path: "no-leading-slash" });
    expect(errs["path"]).toMatch(/Format invalide/);
  });

  it("accepte une string matchant le pattern", () => {
    const errs = validateParams(webhook, { path: "/hooks/x", method: "POST" });
    expect(errs).toEqual({});
  });

  it("rejette une valeur enum hors options", () => {
    const errs = validateParams(webhook, { path: "/ok", method: "TEAPOT" });
    expect(errs["method"]).toMatch(/Valeur non autoris/);
  });

  it("valide les bornes min sur les numbers", () => {
    const errs = validateParams(polling, { url: "https://x", interval_seconds: 1 });
    expect(errs["interval_seconds"]).toMatch(/≥ 5/);
  });

  it("schedule : cron ou intervalle, fuseau UTC seulement", () => {
    const schedule = findNodeSchema("trigger", "schedule")!;
    expect(defaultParams(schedule)).toEqual({ timezone: "UTC" });
    expect(validateParams(schedule, { cron: "0 9 * * 1-5", timezone: "UTC" })).toEqual({});
    expect(validateParams(schedule, { interval_seconds: 30 })).toEqual({});
    expect(validateParams(schedule, { interval_seconds: 3_000_000 })["interval_seconds"]).toMatch(
      /≤ 2678400/,
    );
    expect(validateParams(schedule, { timezone: "Europe/Paris" })["timezone"]).toMatch(
      /Valeur non autoris/,
    );
  });

  it("polling : champ identifiant par défaut", () => {
    expect(defaultParams(polling)).toEqual({ interval_seconds: 60, key: "id" });
  });

  it("rejette un number non numérique", () => {
    const errs = validateParams(polling, { url: "https://x", interval_seconds: "abc" as unknown as number });
    expect(errs["interval_seconds"]).toMatch(/Doit être un nombre/);
  });

  it("ignore les champs optionnels vides", () => {
    const errs = validateParams(slack, { channel: "#g", text: "hi" });
    expect(errs).toEqual({});
  });

  it("champ JSON : objet ou tableau selon le schéma, expression acceptée (#257)", () => {
    const ok = { parent: { database_id: "db" }, properties: { Name: "x" } };
    expect(validateParams(notion, ok)).toEqual({});
    expect(validateParams(notion, { ...ok, parent: ["db"] })["parent"]).toBe(
      "Objet JSON attendu.",
    );
    expect(validateParams(notion, { ...ok, parent: "{ pas du json" })["parent"]).toBe(
      "JSON invalide.",
    );
    expect(validateParams(notion, { ...ok, parent: 3 })["parent"]).toBe("JSON invalide.");
    expect(validateParams(notion, { ...ok, parent: "{{ trigger.parent }}" })).toEqual({});
    expect(validateParams(notion, { properties: {} })["parent"]).toBe("Champ requis.");
    expect(validateParams(slack, { channel: "#g", text: "hi", blocks: [{}] })).toEqual({});
    expect(validateParams(slack, { channel: "#g", text: "hi", blocks: {} })["blocks"]).toBe(
      "Tableau JSON attendu.",
    );
  });
});

describe("nombre ou expression (#261)", () => {
  const github = findNodeSchema("action", "github.close_issue", CONNECTORS)!;
  const base = { owner: "o", repo: "r" };

  it("isSoleExpression : une seule expression occupant toute la valeur", () => {
    expect(isSoleExpression("{{ hook.body.issue.number }}")).toBe(true);
    expect(isSoleExpression("  {{x}}  ")).toBe(true);
    expect(isSoleExpression("#{{ x }}")).toBe(false);
    expect(isSoleExpression("{{ a }} {{ b }}")).toBe(false);
    expect(isSoleExpression("{{  }}")).toBe(false);
    expect(isSoleExpression("{{}}")).toBe(false);
    expect(isSoleExpression("42")).toBe(false);
  });

  it("parseNumberOrExpression : nombre si la saisie en est un, texte sinon", () => {
    expect(parseNumberOrExpression("42")).toBe(42);
    expect(parseNumberOrExpression(" 7 ")).toBe(7);
    expect(parseNumberOrExpression("{{ x.n }}")).toBe("{{ x.n }}");
    expect(parseNumberOrExpression("abc")).toBe("abc");
    expect(parseNumberOrExpression("   ")).toBeNull();
  });

  it("un entier de connecteur accepte une expression seule, pas du texte", () => {
    expect(validateParams(github, { ...base, issue_number: 12 })).toEqual({});
    expect(
      validateParams(github, { ...base, issue_number: "{{ hook.body.issue.number }}" }),
    ).toEqual({});
    expect(validateParams(github, { ...base, issue_number: "abc" })["issue_number"]).toBe(
      "Nombre ou expression {{ … }} attendu.",
    );
    expect(
      validateParams(github, { ...base, issue_number: "#{{ x.n }}" })["issue_number"],
    ).toBe("Nombre ou expression {{ … }} attendu.");
    expect(validateParams(github, base)["issue_number"]).toBe("Champ requis.");
  });

  it("les nombres des nodes intégrés restent des nombres", () => {
    const polling = findNodeSchema("trigger", "polling")!;
    expect(polling.params.some((p) => p.expression)).toBe(false);
    expect(
      validateParams(polling, { url: "https://x", interval_seconds: "{{ x.n }}" })[
        "interval_seconds"
      ],
    ).toMatch(/Doit être un nombre/);
  });
});

describe("parseJsonInput / jsonDraft (#257)", () => {
  it("transmet un objet ou un tableau, sinon le texte tel quel", () => {
    expect(parseJsonInput('{"a": 1}')).toEqual({ a: 1 });
    expect(parseJsonInput("[1, 2]")).toEqual([1, 2]);
    expect(parseJsonInput("{{ x.items }}")).toBe("{{ x.items }}");
    expect(parseJsonInput('"chaîne"')).toBe('"chaîne"');
    expect(parseJsonInput("  ")).toBeNull();
  });

  it("affiche le texte saisi, ou la valeur stockée en JSON indenté", () => {
    expect(jsonDraft("{{ x }}")).toBe("{{ x }}");
    expect(jsonDraft({ a: 1 })).toBe('{\n  "a": 1\n}');
    expect(jsonDraft(null)).toBe("");
    expect(jsonDraft(undefined)).toBe("");
  });
});

describe("extractReferences", () => {
  it("extrait une référence simple sans path", () => {
    const refs = extractReferences("{{ $node('trigger-1') }}");
    expect(refs).toEqual([{ raw: "{{ $node('trigger-1') }}", nodeId: "trigger-1" }]);
  });

  it("extrait nodeId + path", () => {
    const refs = extractReferences("Hello {{ $node('http-1').output.items }}");
    expect(refs).toEqual([
      {
        raw: "{{ $node('http-1').output.items }}",
        nodeId: "http-1",
        path: "output.items",
      },
    ]);
  });

  it("accepte les guillemets simples et doubles", () => {
    const refs = extractReferences('{{ $node("a") }} et {{ $node(\'b\').x }}');
    expect(refs.map((r) => r.nodeId)).toEqual(["a", "b"]);
  });

  it("renvoie [] sur une valeur non-string ou vide", () => {
    expect(extractReferences(undefined)).toEqual([]);
    expect(extractReferences(42)).toEqual([]);
    expect(extractReferences("")).toEqual([]);
    expect(extractReferences("pas de ref")).toEqual([]);
  });

  it("trouve plusieurs références dans la même chaîne", () => {
    const v = "x={{ $node('a').f }} y={{ $node('b').g }} z={{ $node('a') }}";
    expect(extractReferences(v).map((r) => r.nodeId)).toEqual(["a", "b", "a"]);
  });
});

describe("coerceNumberInput", () => {
  it("renvoie null pour une string vide", () => {
    expect(coerceNumberInput("")).toBeNull();
    expect(coerceNumberInput("   ")).toBeNull();
  });
  it("renvoie un number pour une string parsable", () => {
    expect(coerceNumberInput("42")).toBe(42);
    expect(coerceNumberInput("3.14")).toBeCloseTo(3.14);
  });
  it("renvoie null pour une string non numérique", () => {
    expect(coerceNumberInput("abc")).toBeNull();
  });
});
