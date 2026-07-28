/// <reference types="vite/client" />

interface ImportMetaEnv {
	/** API origin baked in at build time — see `src/lib/api.ts`. */
	readonly VITE_AMD_API?: string;
}

interface ImportMeta {
	readonly env: ImportMetaEnv;
}
