#!/usr/bin/env node
// 第三方授權聲明（流程見 docs/release.md「授權與第三方聲明」）：
//   node scripts/notices.mjs           產生 THIRD-PARTY-NOTICES.txt
//   node scripts/notices.mjs --check   核對 THIRD-PARTY-NOTICES.txt 與目前的 Cargo.lock、node_modules 一致，不一致就非 0 結束
// Rust crate 部分由 cargo-about 依 src-tauri/about.toml 與 src-tauri/about.hbs 產生；cargo-about 看不到、但也連結進 exe 的
// 元件（Rust 標準函式庫、WebView2 SDK 的 loader）用 src-tauri/licenses/ 下逐字保存的授權檔；前端部分讀 node_modules 裡會打包進
// dist 的套件。
// 輸出只取決於輸入：不寫時間戳、不寫本機路徑，換行一律 LF（同 repo 其他檔案）。
// cargo-about 的任何 WARN／ERROR 都當失敗：clarify 的 checksum 對不上時它只印 WARN、改用範本全文，不會自己失敗。
// 只用 Node 內建模組；子程序一律 argv 陣列、不經 shell。
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const TAURI_DIR = join(ROOT, "src-tauri");
const OUTPUT_NAME = "THIRD-PARTY-NOTICES.txt";
const OUTPUT = join(ROOT, OUTPUT_NAME);
/** 只發 Windows x64；與 about.toml 的 targets 相同。 */
const TARGET = "x86_64-pc-windows-msvc";
/** 不同版本的 cargo-about 掃描結果與模板資料可能不同，--check 會因工具而非依賴而失敗；鎖定版本，升級時一併重產。 */
const CARGO_ABOUT_VERSION = "0.9.2";
const INSTALL_HINT = `cargo install cargo-about --locked --features cli --version ${CARGO_ABOUT_VERSION}`;
const RULE = "=".repeat(80);
const THIN_RULE = "-".repeat(80);
/** 逐字保存的授權檔與來源記錄（vendored.json），說明見 docs/release.md「授權與第三方聲明」。 */
const VENDORED_DIR = join(TAURI_DIR, "licenses");

/**
 * npm 套件沒附授權全文時改用的來源。@tauri-apps/plugin-updater 的 npm 套件只有 LICENSE.spdx（SPDX 標示與版權行，
 * 沒有授權全文）；同一份原始碼（tauri-apps/plugins-workspace 的 plugins/updater）也發佈成 Rust crate
 * tauri-plugin-updater，crate 附有 LICENSE_APACHE-2.0 與 LICENSE_MIT。兩者版本相同才採用，否則失敗、要人重新確認。
 */
const LICENSE_FROM_CRATE = new Map([["@tauri-apps/plugin-updater", "tauri-plugin-updater"]]);

/**
 * 不是 runtime 依賴、但建置時把自己的程式碼放進 dist 的套件。
 * vite：vite.config.ts 沒關 build.modulePreload.polyfill，dist 的 JS 開頭是 vite 的 modulepreload polyfill。
 * vite 的 LICENSE.md 前段是 vite 本身的 MIT，後段是 vite 發佈檔內含的依賴（只在建置時執行，不進 dist），只取前段。
 */
const BUILD_INJECTED = [
  {
    name: "vite",
    what: "modulepreload polyfill injected into the bundle / 建置時注入的 modulepreload polyfill",
    file: "LICENSE.md",
    until: "\n# Licenses of bundled dependencies",
  },
];

/**
 * @param {string} message
 * @returns {never}
 */
function fail(message) {
  console.error(`notices: ${message}`);
  process.exit(1);
}

/**
 * 換行統一成 LF，去掉結尾的空白行。
 * @param {string} text
 */
function normalize(text) {
  return text.replace(/\r\n?/g, "\n").replace(/\s+$/, "");
}

/**
 * @param {string} cmd
 * @param {string[]} args
 * @param {string} what
 */
function run(cmd, args, what) {
  const result = spawnSync(cmd, args, { cwd: TAURI_DIR, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  if (result.error) fail(`${what} 無法執行：${result.error.message}`);
  return result;
}

function checkCargoAbout() {
  const result = run("cargo", ["about", "--version"], "cargo about");
  if (result.status !== 0) fail(`找不到 cargo-about，先安裝：${INSTALL_HINT}`);
  const version = result.stdout.trim().split(/\s+/).pop();
  if (version !== CARGO_ABOUT_VERSION) {
    fail(`cargo-about 是 ${version}，這份聲明要用 ${CARGO_ABOUT_VERSION} 產生：${INSTALL_HINT} --force`);
  }
}

/** @returns {string} Rust 部分（cargo-about 依 about.hbs 產生） */
function rustSection() {
  const dir = mkdtempSync(join(tmpdir(), "xlm-notices-"));
  try {
    const out = join(dir, "rust.txt");
    // --locked：不准改 Cargo.lock；--fail：授權式無法解析或滿足就失敗。不加 --offline：about.toml 有幾筆 clarify
    // 要從 crate 的原始 repo 取授權檔（套件本身沒附）。
    const result = run(
      "cargo",
      [
        "about",
        "generate",
        "--locked",
        "--fail",
        "--manifest-path",
        join(TAURI_DIR, "Cargo.toml"),
        "--config",
        join(TAURI_DIR, "about.toml"),
        "--output-file",
        out,
        join(TAURI_DIR, "about.hbs"),
      ],
      "cargo about generate",
    );
    const diagnostics = result.stderr
      .split(/\r?\n/)
      .filter((line) => /\[(WARN|ERROR)\]|^\s*(error|warning)(\[|:)/.test(line));
    if (result.status !== 0 || diagnostics.length > 0) {
      console.error(result.stderr.trim());
      fail(
        "cargo-about 失敗或有警告。授權不在 src-tauri/about.toml 的 accepted：確認義務後再加入；" +
          "clarify 驗證失敗（crate 升版、授權檔變了，或連不上 crate 的原始 repo）：重新檢查該 crate 的授權後更新 checksum。" +
          "做法見 docs/release.md「授權與第三方聲明」。",
      );
    }
    return normalize(readFileSync(out, "utf8"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/**
 * 聲明裡的一個元件；version 為 null 時不標版本。
 * @typedef {{ name: string; version: string | null; license: string; note: string | null; files: { label: string; text: string }[] }} Component
 * @typedef {Component & { version: string }} NpmPackage
 */

/**
 * 依 Node 的解析規則找套件目錄：由 from 往上找 node_modules/<name>，最遠到 repo 根。
 * @param {string} name
 * @param {string} from
 */
function resolvePackageDir(name, from) {
  for (let dir = from; ; dir = dirname(dir)) {
    const candidate = join(dir, "node_modules", ...name.split("/"));
    if (existsSync(join(candidate, "package.json"))) return candidate;
    if (dir === ROOT || dirname(dir) === dir) return null;
  }
}

/**
 * @typedef {{ name: string; version: string; license?: unknown; dependencies?: Record<string, string>; optionalDependencies?: Record<string, string> }} PackageJson
 */

/**
 * @param {string} dir
 * @returns {PackageJson}
 */
function readPackageJson(dir) {
  return JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
}

/**
 * 套件根目錄的授權檔（LICENSE*／LICENCE*／COPYING*／NOTICE*）；.spdx 只有標示沒有全文，不算。
 * @param {string} dir
 */
function licenseFiles(dir) {
  return readdirSync(dir)
    .filter((f) => /^(licen[cs]e|copying|notice)/i.test(f) && !/\.spdx$/i.test(f))
    .sort()
    .map((f) => {
      const text = normalize(readFileSync(join(dir, f), "utf8"));
      if (!text) fail(`${dir} 的 ${f} 是空檔`);
      return { label: f, text };
    });
}

/** @type {Map<string, Map<string, string>> | null} crate 名稱 → 版本 → 原始碼目錄 */
let crateIndex = null;

/**
 * Cargo.lock 裡（只算發佈目標平台）某個 crate 的各版本與原始碼目錄（cargo metadata 的 manifest_path）。
 * @param {string} name
 * @returns {Map<string, string>}
 */
function crateVersions(name) {
  if (crateIndex === null) {
    const result = run(
      "cargo",
      ["metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET, "--manifest-path", join(TAURI_DIR, "Cargo.toml")],
      "cargo metadata",
    );
    if (result.status !== 0) fail(`cargo metadata 失敗：${result.stderr.trim()}`);
    crateIndex = new Map();
    for (const pkg of JSON.parse(result.stdout).packages) {
      const versions = crateIndex.get(pkg.name) ?? new Map();
      versions.set(pkg.version, dirname(pkg.manifest_path));
      crateIndex.set(pkg.name, versions);
    }
  }
  return crateIndex.get(name) ?? new Map();
}

/**
 * @param {string} dir
 * @param {string | null} note
 * @returns {NpmPackage}
 */
function npmPackage(dir, note) {
  const pkg = readPackageJson(dir);
  const where = `node_modules 的 ${pkg.name}`;
  if (typeof pkg.license !== "string" || !pkg.license) fail(`${where} 的 package.json 沒有 license 字串`);
  let files = licenseFiles(dir);
  if (files.length === 0) {
    const crate = LICENSE_FROM_CRATE.get(pkg.name);
    if (!crate) fail(`${where}（${pkg.version}）找不到授權檔；確認授權全文的來源後，在 scripts/notices.mjs 補上對應`);
    const source = crateVersions(crate).get(pkg.version);
    if (!source) {
      fail(`${where} ${pkg.version} 沒附授權全文，原本改用 Rust crate ${crate} 的，但 Cargo.lock 裡沒有同版本的 ${crate}；重新確認來源`);
    }
    files = licenseFiles(source).map((f) => ({ label: `${f.label} (from the Rust crate ${crate} ${pkg.version} / 取自同版本的 Rust crate ${crate})`, text: f.text }));
    if (files.length === 0) fail(`${where} 與 Rust crate ${crate} ${pkg.version} 都找不到授權檔`);
  }
  return { name: pkg.name, version: pkg.version, license: pkg.license, note, files };
}

/** @returns {NpmPackage[]} 打包進 dist 的套件：package.json 的 dependencies 與其遞移依賴，加上 BUILD_INJECTED。 */
function npmPackages() {
  /** @type {Map<string, NpmPackage>} 以目錄為鍵：同名不同版本各算一個 */
  const found = new Map();
  /** @param {PackageJson} pkg @param {string} from */
  const visit = (pkg, from) => {
    for (const name of Object.keys({ ...pkg.dependencies, ...pkg.optionalDependencies }).sort()) {
      const dir = resolvePackageDir(name, from);
      if (!dir) {
        if (pkg.optionalDependencies?.[name] !== undefined) continue;
        fail(`node_modules 裡沒有 ${name}：先執行 npm ci`);
      }
      if (found.has(dir)) continue;
      found.set(dir, npmPackage(dir, null));
      visit(readPackageJson(dir), dir);
    }
  };
  visit(readPackageJson(ROOT), ROOT);

  /** @param {string} a @param {string} b */
  const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  const runtime = [...found.values()].sort((a, b) => cmp(a.name, b.name) || cmp(a.version, b.version));
  const injected = BUILD_INJECTED.map(({ name, what, file, until }) => {
    const dir = resolvePackageDir(name, ROOT) ?? fail(`node_modules 裡沒有 ${name}：先執行 npm ci`);
    const pkg = npmPackage(dir, what);
    const path = join(dir, file);
    if (!existsSync(path)) fail(`${name} 沒有 ${file}`);
    const text = readFileSync(path, "utf8").replace(/\r\n?/g, "\n");
    const end = text.indexOf(until);
    if (end < 0) fail(`${name} 的 ${file} 找不到「${until.trim()}」，格式變了，重新確認要取哪一段`);
    return { ...pkg, files: [{ label: `${file} (${name} itself / ${name} 本身的部分)`, text: normalize(text.slice(0, end)) }] };
  });
  return [...runtime, ...injected];
}

/**
 * @typedef {{ path: string; sha256: string }} VendoredFile
 * @typedef {{ name: string; version: string | null; license: string; note: string; source: string; sourceSha256: string | null; boundTo: { crate: string; version: string } | null; files: VendoredFile[] }} VendoredEntry
 */

/**
 * cargo-about 看不到、但連結進 exe 的元件：授權檔逐字保存在 src-tauri/licenses/，來源與 SHA-256 記在 vendored.json。
 * - 檔案內容（換行統一成 LF 後）的 SHA-256 與記錄不同就失敗：保存的檔被改過或換行被轉過以外的變動。
 * - boundTo：元件隨某個 crate 進來時（WebView2 SDK 由 webview2-com-sys 捆綁），Cargo.lock 裡該 crate 的版本要與記錄相同，
 *   否則失敗——crate 升版可能換了 SDK 版本與授權，要重新取原文核對。
 * - Rust 標準函式庫不綁 toolchain 版本：授權（Apache-2.0 OR MIT）與這幾份檔案多年未變，綁版本會讓每次 toolchain 更新都要重產。
 * @returns {Component[]}
 */
function vendoredComponents() {
  /** @type {VendoredEntry[]} */
  const entries = JSON.parse(readFileSync(join(VENDORED_DIR, "vendored.json"), "utf8"));
  return entries.map((entry) => {
    if (entry.boundTo) {
      const { crate, version } = entry.boundTo;
      const locked = [...crateVersions(crate).keys()];
      if (locked.length !== 1 || locked[0] !== version) {
        fail(
          `${entry.name} 記錄為隨 ${crate} ${version} 附帶，但 Cargo.lock 裡的 ${crate} 是 ${locked.join("、") || "（沒有）"}：` +
            "重新查該版捆綁的版本、取回授權原文後更新 src-tauri/licenses/（做法見 docs/release.md「授權與第三方聲明」）",
        );
      }
    }
    const files = entry.files.map(({ path, sha256 }) => {
      const text = readFileSync(join(VENDORED_DIR, path), "utf8").replace(/\r\n?/g, "\n");
      const actual = createHash("sha256").update(text).digest("hex");
      if (actual !== sha256) fail(`src-tauri/licenses/${path} 的 SHA-256 是 ${actual}，與 vendored.json 記錄的 ${sha256} 不同：保存的授權原文不可修改`);
      return { label: path.split("/").pop() ?? path, text: normalize(text) };
    });
    return { name: entry.name, version: entry.version, license: entry.license, note: entry.note, files };
  });
}

/**
 * 一個部分：先列「授權 → 元件」摘要，再列各元件的授權全文。
 * @param {string[]} heading
 * @param {string[]} textsHeading
 * @param {Component[]} components
 */
function section(heading, textsHeading, components) {
  /** @param {Component} c */
  const title = (c) => (c.version ? `${c.name} ${c.version}` : c.name);
  /** @type {Map<string, Component[]>} */
  const byLicense = new Map();
  for (const c of components) byLicense.set(c.license, [...(byLicense.get(c.license) ?? []), c]);
  const summary = [...byLicense.entries()]
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .flatMap(([license, list]) => ["", license, ...list.map((c) => `    ${title(c)}${c.note ? ` (${c.note})` : ""}`)]);
  const texts = components.flatMap((c) =>
    c.files.flatMap((f) => ["", THIN_RULE, `${title(c)} (${c.license})`, `File / 檔案：${f.label}`, THIN_RULE, "", f.text]),
  );
  return [
    RULE,
    ...heading,
    RULE,
    "",
    "Summary: license -> components using it",
    "摘要：授權 → 使用它的元件",
    THIN_RULE,
    ...summary,
    "",
    RULE,
    ...textsHeading,
    RULE,
    ...texts,
  ].join("\n");
}

function render() {
  checkCargoAbout();
  const header = [
    "XIV Launch Manager - Third-Party Notices",
    "XIV Launch Manager 第三方授權聲明",
    "",
    "XIV Launch Manager is licensed under the Apache License 2.0 (see LICENSE and NOTICE).",
    "This file lists the third-party components distributed with it and their licenses.",
    "XIV Launch Manager 以 Apache License 2.0 授權（見 LICENSE 與 NOTICE）。",
    "本檔列出隨本程式發佈的第三方元件及其授權。",
    "",
    "Generated by scripts/notices.mjs; do not edit by hand.",
    "本檔由 scripts/notices.mjs 產生，請勿手動修改。",
  ].join("\n");
  const linked = section(
    ["Part 2. Other components linked into the program", "第二部分：其他連結進主程式的元件"],
    ["Other linked components: full license texts", "其他連結元件授權全文"],
    vendoredComponents(),
  );
  const frontend = section(
    ["Part 3. Frontend packages bundled into the program's user interface", "第三部分：打包進主程式介面的前端套件"],
    ["Frontend packages: full license texts", "前端套件授權全文"],
    npmPackages(),
  );
  return `${[header, rustSection(), linked, frontend].join("\n\n\n")}\n`;
}

/**
 * @param {string} expected
 * @param {string} actual
 */
function firstDifference(expected, actual) {
  const a = expected.split("\n");
  const b = actual.split("\n");
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if (a[i] !== b[i]) return { line: i + 1, expected: a[i] ?? "（檔案結束）", actual: b[i] ?? "（檔案結束）" };
  }
  return null;
}

const args = process.argv.slice(2);
const check = args.includes("--check");
const unknown = args.filter((a) => a !== "--check");
if (unknown.length > 0) fail(`不認得的參數 ${unknown.join(" ")}（用法：node scripts/notices.mjs [--check]）`);

const content = render();
if (check) {
  if (!existsSync(OUTPUT)) fail(`沒有 ${OUTPUT_NAME}：執行 node scripts/notices.mjs 產生後提交`);
  // 比對內容；工作樹的換行可能因 git 設定變成 CRLF，不算差異。
  const diff = firstDifference(content, readFileSync(OUTPUT, "utf8").replace(/\r\n?/g, "\n"));
  if (diff) {
    console.error(`notices: ${OUTPUT_NAME} 第 ${diff.line} 行與目前的依賴不符`);
    console.error(`  應為：${diff.expected}`);
    console.error(`  實際：${diff.actual}`);
    fail(`${OUTPUT_NAME} 過期：執行 node scripts/notices.mjs 重新產生，檢查差異後提交`);
  }
  console.log(`notices: ${OUTPUT_NAME} 與目前的 Cargo.lock、node_modules 一致`);
} else {
  writeFileSync(OUTPUT, content);
  console.log(`notices: 已寫入 ${OUTPUT_NAME}`);
}
