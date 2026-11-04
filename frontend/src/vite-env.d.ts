/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_DOTCORE_MANAGEMENT_ORIGIN?: string;
  readonly VITE_DOTCORE_INFERENCE_ORIGIN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
