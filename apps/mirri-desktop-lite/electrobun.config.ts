import type { ElectrobunConfig } from "electrobun";

export default {
	app: {
		name: "Mirri Code Lite",
		identifier: "ai.mirri.desktop-lite",
		version: "0.1.0",
	},
	build: {
		mainProcess: "bun",
		bun: {
			entrypoint: "src/bun/index.ts",
		},
		// The shell always loads the web UI from the daemon over http; the
		// bundled view is the self-check/test page only.
		copy: {
			"dist/index.html": "views/mainview/index.html",
			"dist/assets": "views/mainview/assets",
			"dist/preload.js": "preload.js",
		},
		watchIgnore: ["dist/**"],
		mac: {
			bundleCEF: false,
		},
		linux: {
			bundleCEF: false,
		},
		win: {
			bundleCEF: false,
		},
	},
} satisfies ElectrobunConfig;
