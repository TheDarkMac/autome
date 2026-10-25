import { afterEach, describe, expect, it, vi } from "vitest";
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
  afterEach(() => {
    delete window.__AUTOME_CONFIG__;
    vi.unstubAllEnvs();
  });

  it("falls back to localhost when neither runtime nor build config is set", () => {
    vi.stubEnv("VITE_API_URL", "");
    expect(apiBaseUrl()).toBe("http://localhost:8080");
  });

  it("uses VITE_API_URL when the runtime config has no apiUrl", () => {
    // `public/config.js` par défaut (dev, `vite`) : objet vide.
    window.__AUTOME_CONFIG__ = {};
    vi.stubEnv("VITE_API_URL", "http://localhost:9000");
    expect(apiBaseUrl()).toBe("http://localhost:9000");
  });

  it("prefers the runtime config written at container start (#282)", () => {
    window.__AUTOME_CONFIG__ = { apiUrl: "https://api.staging.example.com" };
    vi.stubEnv("VITE_API_URL", "http://localhost:9000");
    expect(apiBaseUrl()).toBe("https://api.staging.example.com");
  });

  it("ignores an empty runtime apiUrl", () => {
    window.__AUTOME_CONFIG__ = { apiUrl: "" };
    vi.stubEnv("VITE_API_URL", "http://localhost:9000");
    expect(apiBaseUrl()).toBe("http://localhost:9000");
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
