// @hutch cli=0.28.1 cottontail=0.7.2
export default {
	scripts: {
		install: ["hutch", "install", "--frozen-lockfile"],
		start: "hutch electrobun dev",
		dev: "hutch electrobun dev",
		build: "hutch electrobun build --env=stable",
	},
	electrobun: {
		version: "2.0.4",
	},
};
