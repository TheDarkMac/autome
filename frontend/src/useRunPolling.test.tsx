import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useRunPolling } from "./useRunPolling";
import type { RunCheckpoint } from "./runs";

function makeCheckpoint(over: Partial<RunCheckpoint> = {}): RunCheckpoint {
  return {
    run_id: "r-1",
    workflow_id: "wf-x",
    version: 1,
    trigger_type: "webhook",
    status: "Running",
    started_at_ms: 1_000,
    finished_at_ms: null,
    created_at_ms: 800,
    outputs: {},
    next_node: "n-2",
    error: null,
    ...over,
  };
}

// On évite `waitFor` (qui s'appuie sur de vrais setTimeout) et on pilote
// directement les fake timers : chaque assertion suit un
// `await act(() => advanceTimersByTimeAsync(...))` qui flush microtasks +
// timers.

describe("useRunPolling", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("retourne tout à null quand runId est null (pas de fetch)", () => {
    const fetchImpl = vi.fn();
    const { result } = renderHook(() => useRunPolling(null, { fetchImpl }));
    expect(result.current.checkpoint).toBeNull();
    expect(result.current.error).toBeNull();
    expect(result.current.loading).toBe(false);
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("récupère le checkpoint initial dès que runId est fourni", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(makeCheckpoint({ status: "Running" }));
    const { result } = renderHook(() =>
      useRunPolling("r-1", { fetchImpl, intervalMs: 100 }),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetchImpl).toHaveBeenCalledWith("r-1");
    expect(result.current.checkpoint?.run_id).toBe("r-1");
    expect(result.current.loading).toBe(false);
  });

  it("re-fetch à chaque intervalle tant que le run n'est pas terminal", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(makeCheckpoint({ status: "Running" }));
    renderHook(() => useRunPolling("r-1", { fetchImpl, intervalMs: 50 }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(2);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(3);
  });

  it("arrête le polling dès qu'un statut terminal est observé", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(makeCheckpoint({ status: "Running" }))
      .mockResolvedValueOnce(
        makeCheckpoint({ status: "Succeeded", finished_at_ms: 5_000 }),
      )
      .mockResolvedValueOnce(makeCheckpoint({ status: "Running" }));
    const { result } = renderHook(() =>
      useRunPolling("r-1", { fetchImpl, intervalMs: 50 }),
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    expect(result.current.checkpoint?.status).toBe("Running");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(2);
    expect(result.current.checkpoint?.status).toBe("Succeeded");

    // Plus aucun tick après le statut terminal.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });

  it("expose l'erreur sur fetch raté puis recommence au tick suivant", async () => {
    const fetchImpl = vi
      .fn()
      .mockRejectedValueOnce(new Error("temporaire"))
      .mockResolvedValue(
        makeCheckpoint({ status: "Succeeded", finished_at_ms: 2_000 }),
      );
    const { result } = renderHook(() =>
      useRunPolling("r-1", { fetchImpl, intervalMs: 25 }),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.error?.message).toBe("temporaire");
    expect(result.current.checkpoint).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(25);
    });
    expect(result.current.checkpoint?.status).toBe("Succeeded");
    expect(result.current.error).toBeNull();
  });

  it("redémarre proprement quand runId change", async () => {
    const fetchImpl = vi
      .fn()
      .mockImplementation((id: string) =>
        Promise.resolve(makeCheckpoint({ run_id: id, status: "Running" })),
      );
    const { result, rerender } = renderHook(
      ({ id }: { id: string | null }) =>
        useRunPolling(id, { fetchImpl, intervalMs: 100 }),
      { initialProps: { id: "r-1" } },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.checkpoint?.run_id).toBe("r-1");

    rerender({ id: "r-2" });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.checkpoint?.run_id).toBe("r-2");
    expect(fetchImpl).toHaveBeenCalledWith("r-2");
  });

  it("arrête le polling au unmount", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(makeCheckpoint({ status: "Running" }));
    const { unmount } = renderHook(() =>
      useRunPolling("r-1", { fetchImpl, intervalMs: 25 }),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);

    unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });
});
