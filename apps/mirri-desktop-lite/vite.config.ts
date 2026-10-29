import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import { resolve } from "node:path";
import { electrobunViteAliases } from "./.hutch/devkit/api/config/electrobun-vite";

export default defineConfig({
	plugins: [vue()],
	resolve: {
		alias: [
			...electrobunViteAliases(resolve(__dirname, ".hutch/devkit")),
			// the web bridge contract — type-only, single source of truth (ADR-001)
			
		],
	},
	root: "src/mainview",
	build: {
		outDir: "../../dist",
		emptyOutDir: true,
	},
	server: {
		port: 5174,
		strictPort: true,
	},
});
