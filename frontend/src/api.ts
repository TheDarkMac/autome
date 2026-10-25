// Helper fetch vers le backend Autome (F8.1.c).
//
// `apiBaseUrl()` lit d'abord la configuration d'exécution (`/config.js`, écrit
// au démarrage du conteneur depuis `AUTOME_API_URL`, #282) : une même image
// sert staging et production. Puis `VITE_API_URL`, inliné à la build (dev,
// `.env.local`) ; le default `http://localhost:8080` reste pratique pour le dev
// sans `.env.local`. `fetchOpenApi()` est le premier appel exposé — il sert de
// canari à l'intégration backend pour F8.1.

/** Forme minimale de la spec OpenAPI 3 qu'on attend du backend (F7.4). */
export type OpenApiSpec = {
  openapi: string;
  info: { title: string; version: string };
  paths: Record<string, unknown>;
};

/** Base URL du backend — config d'exécution, sinon `VITE_API_URL`. */
export function apiBaseUrl(): string {
  return (
    window.__AUTOME_CONFIG__?.apiUrl ||
    import.meta.env.VITE_API_URL ||
    "http://localhost:8080"
  );
}

/** Erreur HTTP/transport renvoyée par les helpers — sans secret du token. */
export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status?: number,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** Récupère `GET /openapi.json` du backend (F8.1.c canari). */
export async function fetchOpenApi(
  fetchImpl: typeof fetch = fetch,
): Promise<OpenApiSpec> {
  let response: Response;
  try {
    response = await fetchImpl(`${apiBaseUrl()}/openapi.json`, {
      headers: { Accept: "application/json" },
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (!response.ok) {
    throw new ApiError(
      `GET /openapi.json a échoué (${response.status})`,
      response.status,
    );
  }
  return (await response.json()) as OpenApiSpec;
}
