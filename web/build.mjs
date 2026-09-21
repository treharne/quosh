// Bundles the PWA. esbuild handles TypeScript directly; the wasm-bindgen web
// glue is bundled too, and the three `*_bg.wasm` files are copied next to the
// bundle so the glue's `new URL(..., import.meta.url)` resolves.
import { build } from "esbuild";
import { cp, mkdir, rm } from "node:fs/promises";
import { existsSync } from "node:fs";

const WASM = {
  "pkg/proto/quosh_proto_wasm_bg.wasm": "quosh_proto_wasm_bg.wasm",
  "pkg/predict/quosh_predict_wasm_bg.wasm": "quosh_predict_wasm_bg.wasm",
  "pkg/client/quosh_client_wasm_bg.wasm": "quosh_client_wasm_bg.wasm",
};

for (const src of Object.keys(WASM)) {
  if (!existsSync(src)) {
    console.error(`missing ${src}; run tools/build-wasm.sh first`);
    process.exit(1);
  }
}

await rm("dist", { recursive: true, force: true });
await mkdir("dist", { recursive: true });

await build({
  entryPoints: ["src/app.ts"],
  bundle: true,
  format: "esm",
  target: "es2022",
  outfile: "dist/app.js",
  sourcemap: true,
  logLevel: "info",
});

await cp("index.html", "dist/index.html");
await cp("style.css", "dist/style.css");
for (const [src, name] of Object.entries(WASM)) await cp(src, `dist/${name}`);

console.log("built dist/");