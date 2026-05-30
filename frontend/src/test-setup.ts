import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// jsdom n'expose pas certaines APIs DOM modernes utilisées par les libs
// graphiques (React Flow, etc.). On les polyfille avec des stubs neutres —
// suffisant pour le rendu (les tests ne valident pas le layout réel).
class ResizeObserverStub {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}

if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver =
    ResizeObserverStub as unknown as typeof ResizeObserver;
}

if (typeof globalThis.DOMMatrixReadOnly === "undefined") {
  globalThis.DOMMatrixReadOnly = class {
    m22 = 1;
  } as unknown as typeof DOMMatrixReadOnly;
}

// Évite les fuites de DOM entre tests RTL.
afterEach(() => {
  cleanup();
});
