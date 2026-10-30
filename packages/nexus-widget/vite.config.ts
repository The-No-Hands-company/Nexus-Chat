import { defineConfig } from "vite";

export default defineConfig({
  build: {
    lib: {
      entry: "src/widget.ts",
      name: "NexusWidget",
      fileName: "nexus-widget",
      formats: ["iife"],
    },
    outDir: "dist",
    rollupOptions: {
      output: {
        // Self-contained IIFE — no external dependencies
        inlineDynamicImports: true,
      },
    },
    minify: true,
    sourcemap: false,
  },
});
