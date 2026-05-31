// Lecture des credentials disponibles par provider (F8.4.d).
//
// Le NodePanel affiche une dropdown si le schéma déclare `credentialProvider`.
// L'endpoint backend est `GET /credentials?provider=<x>` (cf. F7.3) et
// renvoie une liste `{ id, name, provider }`. On stocke uniquement
// `credential_id` côté params — la valeur sensible (token) reste backend-side.
//
// Comme pour `fetchCatalog`, on retombe **silencieusement** sur `[]` si
// l'endpoint est absent, en erreur de transport, ou mal formé : la palette
// affichera simplement une liste vide + lien vers la gestion des creds.

import { apiBaseUrl } from "./api";

export interface Credential {
  readonly id: string;
  readonly name: string;
  readonly provider: string;
}

interface CredentialsResponse {
  items?: unknown;
}

export async function fetchCredentials(
  provider: string,
  fetchImpl: typeof fetch = fetch,
): Promise<readonly Credential[]> {
  try {
    const url = `${apiBaseUrl()}/credentials?provider=${encodeURIComponent(provider)}`;
    const res = await fetchImpl(url, {
      headers: { Accept: "application/json" },
    });
    if (!res.ok) return [];
    const body = (await res.json()) as CredentialsResponse;
    return parseCredentials(body.items);
  } catch {
    return [];
  }
}

function parseCredentials(raw: unknown): Credential[] {
  if (!Array.isArray(raw)) return [];
  const out: Credential[] = [];
  for (const entry of raw) {
    if (typeof entry !== "object" || entry === null) continue;
    const v = entry as Record<string, unknown>;
    const id = typeof v["id"] === "string" ? v["id"] : null;
    const name = typeof v["name"] === "string" ? v["name"] : null;
    const provider = typeof v["provider"] === "string" ? v["provider"] : null;
    if (id && name && provider) out.push({ id, name, provider });
  }
  return out;
}
