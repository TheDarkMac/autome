import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { App } from "./App";

const sampleSpec = {
  openapi: "3.0.3",
  info: { title: "Autome API", version: "0.1.0" },
  paths: { "/workflows": {}, "/runs/{run_id}": {} },
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("<App />", () => {
  let originalFetch: typeof fetch;
  beforeEach(() => {
    originalFetch = globalThis.fetch;
  });
  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it("shows backend ok state with endpoint count", async () => {
    globalThis.fetch = vi
      .fn()
      .mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    render(<App />);
    expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.getByRole("status")).toHaveTextContent(/Autome API/);
    });
    expect(screen.getByRole("status")).toHaveTextContent(/2 endpoints/);
  });

  it("shows error state when backend is unreachable", async () => {
    globalThis.fetch = vi
      .fn()
      .mockRejectedValue(new Error("ECONNREFUSED")) as typeof fetch;
    render(<App />);
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(/erreur/i);
    });
  });

  it("bascule vers l'onglet Exécutions et monte la RunsList (F8.5.a)", async () => {
    globalThis.fetch = vi.fn(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
      if (url.includes("/workflows/wf-draft/runs")) {
        return jsonResponse({ items: [], total: 0, offset: 0, limit: 50 });
      }
      return new Response("", { status: 404 });
    }) as typeof fetch;
    render(<App />);
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("tab", { name: /Exécutions/ }));
    await waitFor(() =>
      expect(screen.getByTestId("runs-list")).toBeInTheDocument(),
    );
    await waitFor(() =>
      expect(screen.getByText(/Aucun run pour ce workflow/)).toBeInTheDocument(),
    );
  });
});
