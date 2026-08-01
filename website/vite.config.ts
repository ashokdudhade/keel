import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// GitHub Pages project site: https://ashokdudhade.github.io/keel/
export default defineConfig({
  plugins: [react()],
  base: "/keel/",
});
