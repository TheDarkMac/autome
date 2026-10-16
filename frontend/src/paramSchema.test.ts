import { describe, expect, it } from "vitest";

import {
  NODE_SCHEMAS,
  coerceNumberInput,
  defaultParams,
  extractReferences,
  findNodeSchema,
  validateParams,
} from "./paramSchema";

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

  it("attache un credentialProvider aux actions tierces", () => {
    expect(findNodeSchema("action", "slack.send")?.credentialProvider).toBe("slack");
    expect(findNodeSchema("action", "github.create_issue")?.credentialProvider).toBe("github");
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
  const slack = findNodeSchema("action", "slack.send")!;
  const github = findNodeSchema("action", "github.create_issue")!;

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

  it("rejette un number non numérique", () => {
    const errs = validateParams(polling, { url: "https://x", interval_seconds: "abc" as unknown as number });
    expect(errs["interval_seconds"]).toMatch(/Doit être un nombre/);
  });

  it("ignore les champs optionnels vides", () => {
    const errs = validateParams(slack, { channel: "#g", message: "hi" });
    expect(errs).toEqual({});
  });

  it("repère le format repo invalide github", () => {
    const errs = validateParams(github, { repo: "abc", title: "x" });
    expect(errs["repo"]).toMatch(/Format invalide/);
  });

  it("est satisfait quand tous les required sont remplis correctement", () => {
    const errs = validateParams(github, { repo: "TheDarkMac/autome", title: "Bug" });
    expect(errs).toEqual({});
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
