import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { providersToReconnect, useReconnectNeeded } from "./useReconnectNeeded";

function credentials(...entries: Array<[string, boolean]>): unknown {
  return {
    items: entries.map(([provider_id, needs_reconnect]) => ({
      provider_id,
      has_refresh_token: true,
      needs_reconnect,
    })),
  };
}

describe("providersToReconnect", () => {
  it("garde les seules credentials à reconnecter, triées", () => {
    expect(
      providersToReconnect(
        credentials(["slack", true], ["gmail", false], ["github", true]),
      ),
    ).toEqual(["github", "slack"]);
  });

  it("ignore une réponse mal formée", () => {
    expect(providersToReconnect(null)).toEqual([]);
    expect(providersToReconnect({ items: "x" })).toEqual([]);
    expect(
      providersToReconnect({ items: [null, { needs_reconnect: true }, 3] }),
    ).toEqual([]);
  });
});

// Fake timers pilotés à la main (cf. useRunPolling.test.tsx).
describe("useReconnectNeeded", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("sans session : rien, aucune requête", async () => {
    const list = vi.fn();
    const { result } = renderHook(() => useReconnectNeeded(null, "k"));
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(result.current).toEqual([]);
    expect(list).not.toHaveBeenCalled();
  });

  it("relit la liste quand la clé change et à intervalle régulier", async () => {
    const list = vi
      .fn()
      .mockResolvedValueOnce(credentials(["slack", true]))
      .mockResolvedValueOnce(credentials(["slack", false]))
      .mockResolvedValue(credentials(["notion", true]));
    const { result, rerender } = renderHook(
      ({ k }) => useReconnectNeeded(list, k, 1_000),
      { initialProps: { k: "a" } },
    );
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(result.current).toEqual(["slack"]);

    rerender({ k: "b" });
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(result.current).toEqual([]);
    expect(list).toHaveBeenCalledTimes(2);

    await act(() => vi.advanceTimersByTimeAsync(1_000));
    expect(result.current).toEqual(["notion"]);
    expect(list).toHaveBeenCalledTimes(3);
  });

  it("un échec de lecture n'affiche rien", async () => {
    const list = vi
      .fn()
      .mockResolvedValueOnce(credentials(["slack", true]))
      .mockRejectedValue(new Error("503"));
    const { result, rerender } = renderHook(
      ({ k }) => useReconnectNeeded(list, k),
      { initialProps: { k: "a" } },
    );
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(result.current).toEqual(["slack"]);
    rerender({ k: "b" });
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(result.current).toEqual([]);
  });

  it("ignore une réponse arrivée après démontage", async () => {
    let resolve: (v: unknown) => void = () => undefined;
    const list = vi.fn(
      () => new Promise<unknown>((r) => (resolve = r)),
    );
    const { result, unmount } = renderHook(() => useReconnectNeeded(list, "k"));
    unmount();
    await act(async () => resolve(credentials(["slack", true])));
    expect(result.current).toEqual([]);
  });
});
