// Polling d'un run (F8.5.d).
//
// Pas de WS/SSE backend pour l'instant — on re-fetch `GET /runs/{id}`
// à intervalle régulier tant que le statut n'est pas terminal. Une fois
// `Succeeded`/`Failed` atteint, l'intervalle est éteint automatiquement.
//
// `fetchImpl` est paramétrable pour les tests (mock). Le `intervalMs`
// par défaut (2 000 ms) est suffisamment court pour rester réactif sans
// surcharger le backend ; les tests utilisent une valeur fixe.

import { useEffect, useRef, useState } from "react";

import { fetchRun, isTerminalStatus, type RunCheckpoint } from "./runs";

export interface UseRunPollingOptions {
  intervalMs?: number;
  fetchImpl?: (runId: string) => Promise<RunCheckpoint>;
}

export interface UseRunPollingResult {
  checkpoint: RunCheckpoint | null;
  error: Error | null;
  loading: boolean;
}

const DEFAULT_INTERVAL_MS = 2_000;

export function useRunPolling(
  runId: string | null,
  options: UseRunPollingOptions = {},
): UseRunPollingResult {
  const { intervalMs = DEFAULT_INTERVAL_MS, fetchImpl = fetchRun } = options;
  const [checkpoint, setCheckpoint] = useState<RunCheckpoint | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(false);

  // On garde la version actuelle de `fetchImpl` accessible sans
  // re-déclencher l'effet : le runId/intervalMs pilotent les redémarrages.
  const fetchRef = useRef(fetchImpl);
  useEffect(() => {
    fetchRef.current = fetchImpl;
  }, [fetchImpl]);

  useEffect(() => {
    if (runId == null) {
      setCheckpoint(null);
      setError(null);
      setLoading(false);
      return;
    }

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const tick = async (): Promise<void> => {
      setLoading(true);
      try {
        const ck = await fetchRef.current(runId);
        if (cancelled) return;
        setCheckpoint(ck);
        setError(null);
        setLoading(false);
        if (isTerminalStatus(ck.status)) return;
        timer = setTimeout(tick, intervalMs);
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? e : new Error(String(e)));
        setLoading(false);
        // On retente quand même au tick suivant — la cible peut être un
        // 404 transitoire (race avec le dispatcher).
        timer = setTimeout(tick, intervalMs);
      }
    };

    setCheckpoint(null);
    setError(null);
    void tick();

    return () => {
      cancelled = true;
      if (timer !== null) clearTimeout(timer);
    };
  }, [runId, intervalMs]);

  return { checkpoint, error, loading };
}
