import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
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
      expect(screen.getByRole("alert")).toHaveTextContent(/Erreur/);
    });
  });
});
