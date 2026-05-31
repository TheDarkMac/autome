// Couche data-fetching (F8.7.b).
//
// `useApiQuery` est un hook minimal façon « query » : il exécute un `fetcher`
// asynchrone et expose `{ data, loading, error, refetch }`. Un cache mémoire
// partagé (clé → résultat + timestamp) permet :
//   - de réafficher instantanément une donnée déjà chargée au remontage ;
//   - de sauter le réseau tant qu'une entrée est « fraîche » (`staleTimeMs`).
//
// Le hook ignore les résultats périmés (changement de `key` ou démontage en
// vol) via un compteur de requête, et n'appelle jamais `setState` après coup.
//
// Volontairement sans dépendance (pas de react-query) : la surface reste
// petite, testable, et alignée avec le style « fait maison » du frontend.

import { useCallback, useEffect, useRef, useState } from "react";

interface CacheEntry {
  data: unknown;
  storedAtMs: number;
}

const cache = new Map<string, CacheEntry>();

/** Vide le cache : une clé précise, ou tout le cache si `key` est omis. */
export function invalidateQuery(key?: string): void {
  if (key === undefined) cache.clear();
  else cache.delete(key);
}

/** Écrit/écrase une entrée du cache (utile après une mutation). */
export function setQueryData<T>(key: string, data: T, nowMs = Date.now()): void {
  cache.set(key, { data, storedAtMs: nowMs });
}

/** Lit une entrée du cache sans déclencher de requête. */
export function getQueryData<T>(key: string): T | undefined {
  return cache.get(key)?.data as T | undefined;
}

export interface UseApiQueryOptions {
  /** Si `false`, aucune requête n'est lancée (data = cache éventuel). */
  enabled?: boolean;
  /** Durée (ms) pendant laquelle une entrée en cache est servie sans refetch. */
  staleTimeMs?: number;
  /** Horloge injectable (tests). */
  now?: () => number;
}

export interface UseApiQueryResult<T> {
  data: T | null;
  loading: boolean;
  error: Error | null;
  /** Force un rechargement (ignore la fraîcheur du cache). */
  refetch: () => Promise<void>;
}

export function useApiQuery<T>(
  key: string,
  fetcher: () => Promise<T>,
  options: UseApiQueryOptions = {},
): UseApiQueryResult<T> {
  const { enabled = true, staleTimeMs = 0, now = Date.now } = options;

  // `fetcher` peut changer d'identité à chaque rendu — on lit toujours le plus
  // récent sans relancer l'effet pour autant.
  const fetcherRef = useRef(fetcher);
  useEffect(() => {
    fetcherRef.current = fetcher;
  });

  const [data, setData] = useState<T | null>(
    () => (cache.get(key)?.data as T | undefined) ?? null,
  );
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(false);

  // Identifiant de la requête courante : tout résultat dont l'id ne correspond
  // plus est ignoré (clé changée ou hook démonté entre-temps).
  const reqIdRef = useRef(0);

  const run = useCallback(
    async (force: boolean): Promise<void> => {
      const entry = cache.get(key);
      const fresh = entry !== undefined && now() - entry.storedAtMs < staleTimeMs;
      if (!force && fresh) {
        setData(entry.data as T);
        setError(null);
        setLoading(false);
        return;
      }
      const reqId = ++reqIdRef.current;
      setLoading(true);
      setError(null);
      try {
        const result = await fetcherRef.current();
        if (reqId !== reqIdRef.current) return;
        cache.set(key, { data: result, storedAtMs: now() });
        setData(result);
      } catch (e) {
        if (reqId !== reqIdRef.current) return;
        setError(e instanceof Error ? e : new Error(String(e)));
      } finally {
        if (reqId === reqIdRef.current) setLoading(false);
      }
    },
    [key, staleTimeMs, now],
  );

  useEffect(() => {
    // À chaque (re)montage ou changement de clé : resynchronise l'affichage
    // sur l'entrée de cache de la *nouvelle* clé avant de (re)charger.
    setData((cache.get(key)?.data as T | undefined) ?? null);
    setError(null);
    if (!enabled) {
      setLoading(false);
      return;
    }
    void run(false);
    return () => {
      // `reqIdRef` est un compteur de génération (pas un node DOM) : on bump
      // volontairement sa *dernière* valeur pour invalider la requête en vol.
      // eslint-disable-next-line react-hooks/exhaustive-deps
      reqIdRef.current++;
    };
  }, [key, enabled, run]);

  const refetch = useCallback(() => run(true), [run]);

  return { data, loading, error, refetch };
}
