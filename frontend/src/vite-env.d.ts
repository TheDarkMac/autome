/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_API_URL: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

/** Configuration d'exécution, posée par `/config.js` avant le bundle (#282). */
interface AutomeRuntimeConfig {
  readonly apiUrl?: string;
}

interface Window {
  __AUTOME_CONFIG__?: AutomeRuntimeConfig;
}
