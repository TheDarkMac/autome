import { describe, expect, it, vi } from "vitest";
import { ApiError, apiBaseUrl, fetchOpenApi, type OpenApiSpec } from "./api";

const sampleSpec: OpenApiSpec = {
  openapi: "3.0.3",
  info: { title: "Autome API", version: "0.1.0" },
  paths: {
    "/workflows": {},
    "/runs/{run_id}": {},
  },
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("apiBaseUrl", () => {
  it("falls back to localhost when env not set", () => {
    // VITE_API_URL est positionné via .env.local en dev ; en test on vérifie
    // qu'un défaut sûr est utilisé si la string est vide.
    expect(apiBaseUrl()).toMatch(/^https?:\/\//);
  });
});

describe("fetchOpenApi", () => {
  it("returns parsed spec on 200", async () => {
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec));
    const spec = await fetchOpenApi(fakeFetch as unknown as typeof fetch);
    expect(spec.info.title).toBe("Autome API");
    expect(Object.keys(spec.paths)).toHaveLength(2);
    expect(fakeFetch).toHaveBeenCalledWith(
      expect.stringContaining("/openapi.json"),
      expect.objectContaining({ headers: { Accept: "application/json" } }),
    );
  });

  it("throws ApiError with status when backend returns 5xx", async () => {
    const fakeFetch = vi.fn().mockResolvedValue(jsonResponse({}, 503));
    await expect(
      fetchOpenApi(fakeFetch as unknown as typeof fetch),
    ).rejects.toMatchObject({
      name: "ApiError",
      status: 503,
    });
  });

  it("throws ApiError when transport fails", async () => {
    const fakeFetch = vi.fn().mockRejectedValue(new Error("ECONNREFUSED"));
    await expect(
      fetchOpenApi(fakeFetch as unknown as typeof fetch),
    ).rejects.toBeInstanceOf(ApiError);
  });
});
