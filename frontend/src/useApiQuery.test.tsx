import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  getQueryData,
  invalidateQuery,
  setQueryData,
  useApiQuery,
} from "./useApiQuery";

afterEach(() => invalidateQuery());

describe("useApiQuery", () => {
  it("charge au montage : loading puis data", async () => {
    const fetcher = vi.fn(async () => ({ value: 42 }));
    const { result } = renderHook(() => useApiQuery("k1", fetcher));

    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.data).toEqual({ value: 42 });
    expect(result.current.error).toBeNull();
    expect(fetcher).toHaveBeenCalledOnce();
  });

  it("expose l'erreur si le fetcher rejette", async () => {
    const fetcher = vi.fn(async () => {
      throw new Error("boom");
    });
    const { result } = renderHook(() => useApiQuery("k-err", fetcher));

    await waitFor(() => expect(result.current.error).not.toBeNull());
    expect(result.current.error?.message).toBe("boom");
    expect(result.current.data).toBeNull();
    expect(result.current.loading).toBe(false);
  });

  it("refetch recharge et met à jour la donnée", async () => {
    let n = 0;
    const fetcher = vi.fn(async () => ({ n: ++n }));
    const { result } = renderHook(() => useApiQuery("k-refetch", fetcher));

    await waitFor(() => expect(result.current.data).toEqual({ n: 1 }));
    await act(() => result.current.refetch());
    await waitFor(() => expect(result.current.data).toEqual({ n: 2 }));
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it("sert le cache frais sans rappeler le fetcher (staleTimeMs)", async () => {
    setQueryData("k-cache", { cached: true });
    const fetcher = vi.fn(async () => ({ cached: false }));
    const { result } = renderHook(() =>
      useApiQuery("k-cache", fetcher, { staleTimeMs: 60_000 }),
    );

    // Donnée du cache disponible immédiatement, aucun appel réseau.
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.data).toEqual({ cached: true });
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("ne charge pas quand enabled = false", async () => {
    const fetcher = vi.fn(async () => "x");
    const { result } = renderHook(() =>
      useApiQuery("k-disabled", fetcher, { enabled: false }),
    );

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(fetcher).not.toHaveBeenCalled();
    expect(result.current.data).toBeNull();
  });

  it("recharge après invalidation du cache", async () => {
    setQueryData("k-inv", "old");
    const fetcher = vi.fn(async () => "new");
    const { result, rerender } = renderHook(
      ({ stale }: { stale: number }) =>
        useApiQuery("k-inv", fetcher, { staleTimeMs: stale }),
      { initialProps: { stale: 60_000 } },
    );

    await waitFor(() => expect(result.current.data).toBe("old"));
    expect(fetcher).not.toHaveBeenCalled();

    invalidateQuery("k-inv");
    rerender({ stale: 0 });
    await waitFor(() => expect(result.current.data).toBe("new"));
    expect(fetcher).toHaveBeenCalledOnce();
  });

  it("écrit le résultat dans le cache partagé", async () => {
    const fetcher = vi.fn(async () => ({ persisted: 1 }));
    const { result } = renderHook(() => useApiQuery("k-write", fetcher));
    await waitFor(() => expect(result.current.data).toEqual({ persisted: 1 }));
    expect(getQueryData("k-write")).toEqual({ persisted: 1 });
  });

  it("ignore le résultat périmé quand la clé change en vol", async () => {
    const slow = vi.fn(async () => {
      await new Promise((r) => setTimeout(r, 30));
      return "slow-A";
    });
    const fast = vi.fn(async () => "fast-B");
    const { result, rerender } = renderHook(
      ({ k, f }: { k: string; f: () => Promise<string> }) => useApiQuery(k, f),
      { initialProps: { k: "A", f: slow } },
    );

    // Bascule immédiatement sur la clé B (fetcher rapide) avant que A ne résolve.
    rerender({ k: "B", f: fast });
    await waitFor(() => expect(result.current.data).toBe("fast-B"));
    // Laisse A résoudre : ne doit PAS écraser la donnée de B.
    await new Promise((r) => setTimeout(r, 50));
    expect(result.current.data).toBe("fast-B");
  });
});
