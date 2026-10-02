// pdf.js fetches character maps, standard fonts, colour profiles and image
// decoders on demand. Copy them from the package into public/ so Vite serves
// them in development and bundles them into the app.
import { cpSync, rmSync } from "node:fs";

const src = "node_modules/pdfjs-dist";
const dest = "public/pdfjs";
rmSync(dest, { recursive: true, force: true });
for (const dir of ["cmaps", "standard_fonts", "iccs", "wasm"]) {
  cpSync(`${src}/${dir}`, `${dest}/${dir}`, { recursive: true });
}
