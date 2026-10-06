// Copies the installers Tauri buries under target/release/bundle/ into
// release/, with the names they ship under. Used by `npm run pack` and by the
// release workflow, so both produce exactly the same file names.
//
// Windows has always produced an NSIS setup; Linux produces a deb plus an
// AppImage (see `bundle.targets` in tauri.conf.json) — this used to be a
// Windows-only script, so on Linux `npm run pack` failed with "No installer"
// even after a perfectly good build. The targets now follow the platform.

import { readFileSync, mkdirSync, copyFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const bundleRoot = join(root, "target", "release", "bundle");
const outDir = join(root, "release");

const { version } = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8"));

/** Where this platform's installers land, and what they end up called. */
const TARGETS =
  process.platform === "win32"
    ? [
        { dir: "nsis", suffix: "-setup.exe", product: "Windows", required: true },
      ]
    : [
        // Debian's name for x64 is amd64 (and Tauri bundles it that way), so
        // the copies keep that rather than Node's `process.arch`.
        { dir: "deb", suffix: ".deb", product: `Linux-${arch()}`, required: true },
        // Nice to have, never the reason a release fails: the AppImage build
        // needs tooling the deb build does not.
        { dir: "appimage", suffix: ".AppImage", product: `Linux-${arch()}`, required: false },
      ];

function arch() {
  return process.arch === "x64" ? "amd64" : process.arch;
}

let produced = [];
for (const { dir, suffix, product, required } of TARGETS) {
  const searchDir = join(bundleRoot, dir);
  let files = [];
  try {
    files = readdirSync(searchDir).filter((f) => f.endsWith(suffix));
  } catch {
    /* directory missing — handled below */
  }
  if (files.length === 0) {
    console.error(
      required
        ? `No installer in ${searchDir} — run \`npm run tauri build\` first.`
        : `Note: no ${suffix} in ${searchDir} — skipping (only the deb is required).`,
    );
    if (required) process.exit(1);
    continue;
  }

  // Newest wins, in case an older build is still lying around.
  const built = files
    .map((f) => join(searchDir, f))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];

  // The Windows names predate this script and must not drift — a release
  // script that renames artifacts quietly breaks whatever consumes them.
  const name = (withVersion) =>
    process.platform === "win32"
      ? `Coucou-Windows${withVersion ? `-${version}` : ""}${suffix}`
      : `Coco-${product}${withVersion ? `-${version}` : ""}${suffix}`;

  mkdirSync(outDir, { recursive: true });
  const versioned = join(outDir, name(true));
  const rolling = join(outDir, name(false));
  copyFileSync(built, versioned);
  copyFileSync(built, rolling);
  produced.push({ versioned, rolling, size: statSync(versioned).size, source: built });
}

if (produced.length === 0) {
  console.error("Nothing was packaged.");
  process.exit(1);
}

console.log("");
for (const { versioned, rolling, size, source } of produced) {
  console.log(`  ${(size / 1024 / 1024).toFixed(2)} MB  ${versioned}`);
  console.log(`               ${rolling}`);
  console.log(`               (from ${source})\n`);
}
