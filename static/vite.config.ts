import { resolve } from "node:path";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
	plugins: [solid()],
	// Relative asset URLs, so the same build works from a repo-scoped Pages URL
	// (user.github.io/repo/), a custom domain at the root, and file://.
	base: "./",
	build: {
		// Two pages, two entries. Everything they share (the wire types, the
		// formatters, Solid itself) is hoisted into a common chunk by Rollup.
		rollupOptions: {
			input: {
				main: resolve(__dirname, "index.html"),
				status: resolve(__dirname, "status.html"),
			},
		},
	},
	server: { port: 3000 },
	preview: { port: 3000 },
});
