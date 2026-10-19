import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Frontend Autome (F8.1) — Vite + React + Tailwind v4.
// `VITE_API_URL` (cf. .env.example) pointe sur le backend HTTP.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
    css: false,
    // Couverture (#247), active avec `--coverage` (`pnpm run test:coverage`).
    // Tous les sources comptent, testés ou non ; seuls les fichiers sans
    // code exécutable ou hors périmètre jsdom sont exclus.
    coverage: {
      provider: "v8",
      include: ["src/**/*.{ts,tsx}"],
      exclude: [
        "src/**/*.test.{ts,tsx}",
        "src/test-setup.ts",
        "src/vite-env.d.ts",
        // Types générés depuis openapi.json (`pnpm run gen:api`).
        "src/apiSchema.ts",
        // Point d'entrée : montage dans le DOM réel.
        "src/main.tsx",
      ],
      reporter: ["text-summary", "json-summary", "lcov"],
      // Planchers bloquants, juste sous la mesure du 2026-10-20 (lignes
      // 96,1 %, branches 88,6 %, fonctions 91,7 %) : à remonter avec elle.
      thresholds: {
        lines: 95,
        statements: 95,
        branches: 87,
        functions: 90,
      },
    },
  },
});
