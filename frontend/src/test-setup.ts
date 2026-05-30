import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// Évite les fuites de DOM entre tests RTL.
afterEach(() => {
  cleanup();
});
