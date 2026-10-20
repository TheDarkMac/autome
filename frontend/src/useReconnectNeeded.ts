// Connecteurs à reconnecter, signalés hors de l'onglet Connecteurs (#265).
//
// Le worker marque une credential invalide quand le provider refuse son
// refresh token ; `GET /credentials` l'expose (`needs_reconnect`, #213). Ce
// hook relit la liste à chaque changement de `refreshKey` (session, onglet,
// fin de run, reconnexion) et toutes les `RECONNECT_POLL_MS`, et renvoie les
// providers concernés pour l'en-tête. Un échec de lecture n'affiche rien :
// l'indicateur ne doit jamais signaler un faux problème.

import { useEffect, useState } from "react";

/** Relecture périodique (les runs planifiés invalident sans action de l'UI). */
export const RECONNECT_POLL_MS = 5 * 60 * 1000;

/** Providers dont la credential est à reconnecter, triés. */
export function providersToReconnect(body: unknown): string[] {
  const items =
    typeof body === "object" && body !== null
      ? (body as Record<string, unknown>)["items"]
      : undefined;
  if (!Array.isArray(items)) return [];
  const out: string[] = [];
  for (const it of items) {
    if (typeof it !== "object" || it === null) continue;
    const v = it as Record<string, unknown>;
    if (v["needs_reconnect"] === true && typeof v["provider_id"] === "string") {
      out.push(v["provider_id"]);
    }
  }
  return out.sort();
}

/**
 * Providers à reconnecter pour l'utilisateur courant. `list` à `null`
 * (pas de session) : liste vide, aucune requête.
 */
export function useReconnectNeeded(
  list: (() => Promise<unknown>) | null,
  refreshKey: string,
  pollMs: number = RECONNECT_POLL_MS,
): readonly string[] {
  const [providers, setProviders] = useState<readonly string[]>([]);
  useEffect(() => {
    if (!list) {
      setProviders([]);
      return;
    }
    let cancelled = false;
    const read = (): void => {
      list().then(
        (body) => {
          if (!cancelled) setProviders(providersToReconnect(body));
        },
        () => {
          if (!cancelled) setProviders([]);
        },
      );
    };
    read();
    const timer = window.setInterval(read, pollMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [list, refreshKey, pollMs]);
  return providers;
}
