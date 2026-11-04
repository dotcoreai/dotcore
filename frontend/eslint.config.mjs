import js from "@eslint/js";
import { defineConfig, globalIgnores } from "eslint/config";
import jsxA11y from "eslint-plugin-jsx-a11y";
import reactPlugin from "eslint-plugin-react";
import reactHooks from "eslint-plugin-react-hooks";
import { reactRefresh } from "eslint-plugin-react-refresh";
import simpleImportSort from "eslint-plugin-simple-import-sort";
import globals from "globals";
import tseslint from "typescript-eslint";

const typeScriptFiles = ["src/**/*.{ts,tsx}", "vite.config.ts"];

export default defineConfig(
  globalIgnores(["node_modules/", "dist/", "coverage/", "src/gen/"]),
  {
    files: typeScriptFiles,
    extends: [js.configs.recommended, tseslint.configs.recommendedTypeChecked],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: {
      "simple-import-sort": simpleImportSort,
    },
    rules: {
      "simple-import-sort/imports": "error",
      "simple-import-sort/exports": "error",
    },
  },
  {
    files: ["src/**/*.{ts,tsx}"],
    languageOptions: {
      globals: globals.browser,
    },
  },
  {
    // Transports are built in one place so cross-cutting request policy applies to every client.
    files: ["src/**/*.{ts,tsx}"],
    ignores: ["src/api/console-transport.ts"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          paths: [
            {
              name: "@connectrpc/connect-web",
              message:
                "Create transports with createConsoleTransport in src/api/console-transport.ts.",
            },
          ],
        },
      ],
    },
  },
  {
    // Shared metric composites take their data as props, so they never depend on a feature.
    files: ["src/components/metrics/**/*.{ts,tsx}"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          paths: [
            {
              name: "@connectrpc/connect-web",
              message:
                "Create transports with createConsoleTransport in src/api/console-transport.ts.",
            },
          ],
          patterns: [
            {
              group: ["@/features/**", "**/features/**"],
              message: "Shared metric components must not import from src/features.",
            },
          ],
        },
      ],
    },
  },
  {
    files: ["src/**/*.{test,spec}.{ts,tsx}", "src/test/**/*.{ts,tsx}"],
    languageOptions: {
      globals: globals.vitest,
    },
  },
  {
    files: ["vite.config.ts"],
    languageOptions: {
      globals: { ...globals.node, ...globals.vitest },
    },
  },
  {
    files: ["src/**/*.tsx"],
    extends: [
      reactPlugin.configs.flat.recommended,
      reactPlugin.configs.flat["jsx-runtime"],
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite(),
      jsxA11y.flatConfigs.recommended,
    ],
    languageOptions: {
      ...jsxA11y.flatConfigs.recommended.languageOptions,
      globals: globals.browser,
    },
    settings: {
      react: {
        version: "detect",
      },
    },
    rules: {
      // TypeScript owns component prop validation.
      "react/prop-types": "off",
      // shadcn exposes these variant builders and hooks as part of a primitive's public API.
      "react-refresh/only-export-components": [
        "error",
        {
          allowConstantExport: true,
          allowExportNames: ["buttonVariants", "badgeVariants", "noticeVariants", "useSidebar"],
        },
      ],
    },
  },
);
