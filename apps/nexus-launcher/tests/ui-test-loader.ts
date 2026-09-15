import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const esbuild = createRequire(require.resolve("vite/package.json"))("esbuild");
const root = fileURLToPath(new URL("../", import.meta.url));
let compiled: Promise<string> | undefined;
let nextModule = 0;

async function compileUi(): Promise<string> {
  try {
    const result = await esbuild.build({
      absWorkingDir: root,
      stdin: { contents: 'export * from "./tests/ui-test-entry.ts";', resolveDir: root, sourcefile: "ui-test-entry.ts" },
      bundle: true, write: false, platform: "node", format: "esm", jsx: "automatic", logLevel: "silent",
      plugins: [{ name: "external-installed-packages", setup(build: { onResolve: Function }) {
        build.onResolve({ filter: /^[^./]/ }, (args: { kind: string; path: string }) => args.kind === "entry-point" ? undefined : { path: import.meta.resolve(args.path), external: true });
      } }],
    });
    return result.outputFiles[0].text;
  } finally { esbuild.stop(); }
}

/** Real components and shared i18n context, without a dev-server transport or business mocks. */
export async function createUiTestLoader() {
  compiled ??= compileUi();
  // Keep each former server's module state isolated while reusing compilation.
  const code = await compiled + "\n// isolated-ui-test-" + (++nextModule);
  const module = await import("data:text/javascript;base64," + Buffer.from(code).toString("base64"));
  return {
    async loadModule(path: string) {
      if (path === "/src/App.tsx") return module;
      if (path === "/src/i18n.ts") return module.__testI18n;
      throw new Error("Unsupported UI test module: " + path);
    },
    // Compilation already closes esbuild; no watcher or transport survives the load.
    async close() {},
  };
}
