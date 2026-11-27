import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { validateLocalizations } from "./scripts/validate-localizations.mjs";
import { validateUIColors } from "./scripts/validate-ui-colors.mjs";

// @ts-expect-error process is supplied by Node while Vite loads this config.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react(), {
    name: "validate-bundled-localizations",
    buildStart() { validateLocalizations(); validateUIColors(); },
    handleHotUpdate(context) {
      if (/\/src\/locales\/(ja|en)\.json$/.test(context.file)) validateLocalizations();
      if (/\/(src\/ui-colors\.json|src\/App\.css)$/.test(context.file)) validateUIColors();
    }
  }],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**", "**/native/**"] }
  }
});

