#!/usr/bin/env node
// 本機發布（流程與金鑰說明見 docs/release.md）：
//   node scripts/release.mjs <version> [--notes <file>]   升版、打包、簽章安裝檔與免安裝 exe、產生 latest.json 到 release/v<version>/
//   node scripts/release.mjs --dry-run [--notes <file>]   不檢查工作樹、不改版本，用目前版本打包驗證
//   node scripts/release.mjs <version> --publish          不重新打包，把 release/v<version>/ 上傳成 GitHub Release
// 只用 Node 內建模組；子程序一律 argv 陣列、不經 shell。
import { execFileSync, spawnSync } from "node:child_process";
import { createHash, createPublicKey, verify } from "node:crypto";
import { copyFileSync, createReadStream, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const REPO = "Yshawn6720/xiv-launch-manager";
/** latest.json 的平台鍵：NSIS 安裝檔用 Tauri 的 OS-ARCH 鍵（舊版程式只認這個）；免安裝 exe 另用一個鍵。 */
const PLATFORM_SETUP = "windows-x86_64";
const PLATFORM_PORTABLE = "windows-x86_64-portable";
const DEFAULT_KEY = join(homedir(), ".tauri", "xiv-launch-manager.key");
const TAURI_CONF = join(ROOT, "src-tauri", "tauri.conf.json");
const NSIS_DIR = join(ROOT, "src-tauri", "target", "release", "bundle", "nsis");
/** 免安裝版就是 tauri build 編出的主程式本身。 */
const PORTABLE_EXE = join(ROOT, "src-tauri", "target", "release", "xiv-launch-manager.exe");
/** 直接跑本機的 tauri CLI（等同 `npx tauri`）：Windows 上 npx 是 .cmd，只能經 shell 執行。 */
const TAURI_CLI = join(ROOT, "node_modules", "@tauri-apps", "cli", "tauri.js");
const NOTES_FILE = "notes.md";
const MANIFEST = "latest.json";
/** 授權與聲明檔，和安裝檔、免安裝 exe 一起上傳到 Release（免安裝版沒有安裝目錄可放）。 */
const NOTICE_FILES = ["LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.txt"];
const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*)?$/;

/** 本套件版本所在的位置：pattern 的三個群組＝前綴、版本、後綴；count＝檔內應有的處數。 */
const VERSION_SITES = [
  { file: "package.json", pattern: /^( {2}"version": ")([^"]+)(")/gm, count: 1 },
  { file: "package-lock.json", pattern: /("name": "xiv-launch-manager",\r?\n\s*"version": ")([^"]+)(")/g, count: 2 },
  { file: "src-tauri/tauri.conf.json", pattern: /^( {2}"version": ")([^"]+)(")/gm, count: 1 },
  { file: "src-tauri/Cargo.toml", pattern: /^(\[package\]\r?\nname = "xiv-launch-manager"\r?\nversion = ")([^"]+)(")/gm, count: 1 },
  { file: "src-tauri/Cargo.lock", pattern: /^(\[\[package\]\]\r?\nname = "xiv-launch-manager"\r?\nversion = ")([^"]+)(")/gm, count: 1 },
];

/**
 * @param {string} message
 * @returns {never}
 */
function fail(message) {
  console.error(`release: ${message}`);
  process.exit(1);
}

/**
 * @param {string[]} argv
 * @returns {{ version: string | null; notes: string | null; publish: boolean; dryRun: boolean }}
 */
function parseArgs(argv) {
  /** @type {{ version: string | null; notes: string | null; publish: boolean; dryRun: boolean }} */
  const opts = { version: null, notes: null, publish: false, dryRun: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--notes") {
      const file = argv[++i];
      if (!file) fail("--notes 後面要接說明檔路徑");
      opts.notes = file;
    } else if (arg === "--publish") opts.publish = true;
    else if (arg === "--dry-run") opts.dryRun = true;
    else if (arg.startsWith("-")) fail(`不認得的參數 ${arg}`);
    else if (opts.version === null) opts.version = arg;
    else fail(`多出的參數 ${arg}`);
  }
  if (opts.publish && opts.dryRun) fail("--publish 與 --dry-run 不能同時用");
  if (opts.publish && opts.notes !== null) fail("--publish 用打包時存下的 notes.md，不接 --notes");
  if (opts.dryRun && opts.version !== null) fail("--dry-run 用目前版本，不接 <version>");
  if (!opts.dryRun && opts.version === null) {
    fail("用法：node scripts/release.mjs <version> [--notes <file>] | <version> --publish | --dry-run [--notes <file>]");
  }
  return opts;
}

/**
 * @param {string} version
 * @returns {{ core: number[]; pre: string[] }}
 */
function parseSemver(version) {
  const m = SEMVER.exec(version);
  if (!m) fail(`版本 ${version} 不是 semver（例如 1.2.3，不加 v 前綴）`);
  return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ? m[4].split(".") : [] };
}

/**
 * semver 優先序（build metadata 不參與）。
 * @param {string} a
 * @param {string} b
 * @returns {number} 負數＝a 較舊
 */
function compareSemver(a, b) {
  const x = parseSemver(a);
  const y = parseSemver(b);
  for (let i = 0; i < 3; i++) {
    if (x.core[i] !== y.core[i]) return x.core[i] - y.core[i];
  }
  // 沒有 pre-release 的版本比有的新。
  if (x.pre.length === 0 || y.pre.length === 0) return Number(y.pre.length > 0) - Number(x.pre.length > 0);
  const numeric = /^\d+$/;
  for (let i = 0; i < Math.min(x.pre.length, y.pre.length); i++) {
    const p = x.pre[i];
    const q = y.pre[i];
    if (p === q) continue;
    const pn = numeric.test(p);
    const qn = numeric.test(q);
    if (pn && qn) return Number(p) - Number(q);
    if (pn !== qn) return pn ? -1 : 1;
    return p < q ? -1 : 1;
  }
  return x.pre.length - y.pre.length;
}

/**
 * 檢查各檔的本套件版本都是 from，回傳改成 to 之後的內容（尚未寫檔）。
 * @param {string} from
 * @param {string} to
 * @returns {{ path: string; text: string }[]}
 */
function versionEdits(from, to) {
  return VERSION_SITES.map(({ file, pattern, count }) => {
    const path = join(ROOT, file);
    let seen = 0;
    const text = readFileSync(path, "utf8").replace(pattern, (_match, pre, current, post) => {
      seen++;
      if (current !== from) fail(`${file} 的版本是 ${current}，與 tauri.conf.json 的 ${from} 不一致`);
      return `${pre}${to}${post}`;
    });
    if (seen !== count) fail(`${file} 找到 ${seen} 處本套件版本，預期 ${count} 處`);
    return { path, text };
  });
}

/** @returns {string} `git status --porcelain` 的輸出；空字串＝工作樹乾淨 */
function gitStatus() {
  return execFileSync("git", ["status", "--porcelain"], { cwd: ROOT, encoding: "utf8" }).trim();
}

/**
 * @param {string} path
 * @returns {Promise<Buffer>}
 */
async function blake2b512File(path) {
  const hash = createHash("blake2b512");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest();
}

/**
 * 以 tauri.conf.json 的 pubkey 驗證 Tauri updater 的 .sig（minisign 格式，base64 包一層），等同 updater 安裝前的檢查。
 * @param {string} file 被簽的檔案
 * @param {string} signature .sig 檔內容
 * @param {string} pubkey tauri.conf.json plugins.updater.pubkey
 * @returns {Promise<string>} trusted comment
 */
async function verifySignature(file, signature, pubkey) {
  const pubLines = Buffer.from(pubkey, "base64").toString("utf8").split(/\r?\n/);
  const pub = Buffer.from(pubLines[1] ?? "", "base64");
  // 公鑰：演算法 "Ed"(2) + key id(8) + Ed25519 公鑰(32)。
  if (pub.length !== 42 || pub.toString("latin1", 0, 2) !== "Ed") fail("tauri.conf.json 的 pubkey 不是 minisign 公鑰");
  const sigLines = Buffer.from(signature, "base64").toString("utf8").split(/\r?\n/);
  const sig = Buffer.from(sigLines[1] ?? "", "base64");
  const trustedLine = sigLines[2] ?? "";
  const globalSig = Buffer.from(sigLines[3] ?? "", "base64");
  const prefix = "trusted comment: ";
  // 簽章：演算法(2) + key id(8) + Ed25519 簽章(64)；"ED"＝對檔案的 BLAKE2b-512 雜湊簽（Tauri CLI 的預設）。
  if (sig.length !== 74 || !trustedLine.startsWith(prefix) || globalSig.length !== 64) fail(".sig 格式不對");
  if (sig.toString("latin1", 0, 2) !== "ED") fail(`.sig 的演算法 ${sig.toString("latin1", 0, 2)} 不是預期的 ED（prehashed）`);
  if (!sig.subarray(2, 10).equals(pub.subarray(2, 10))) {
    fail(".sig 的 key id 與 tauri.conf.json 的 pubkey 不符：簽章用的私鑰不是這把公鑰的配對");
  }
  const key = createPublicKey({ key: { kty: "OKP", crv: "Ed25519", x: pub.subarray(10).toString("base64url") }, format: "jwk" });
  const sigBytes = sig.subarray(10);
  if (!verify(null, await blake2b512File(file), key, sigBytes)) fail(`${file} 的簽章驗證失敗`);
  const trusted = trustedLine.slice(prefix.length);
  if (!verify(null, Buffer.concat([sigBytes, Buffer.from(trusted, "utf8")]), key, globalSig)) fail(".sig 的 trusted comment 驗證失敗");
  return trusted;
}

/**
 * 簽章綁定的版本（trusted comment 的 version: 欄）要與發布版本相同，否則 updater 會拒裝。
 * @param {string} trusted
 * @param {string} version
 */
function checkSignedVersion(trusted, version) {
  const signed = trusted.split("\t").find((field) => field.startsWith("version:"))?.slice("version:".length);
  if (signed !== undefined && signed !== version) fail(`簽章綁定的版本 ${signed} 與發布版本 ${version} 不同`);
}

/**
 * GitHub 上傳時會把檔名的空白換成 "."；先換好，latest.json 的 url 才會對得上實際的 asset 名稱。
 * @param {string} productName
 * @param {string} version
 * @param {"setup" | "portable"} kind
 */
function assetName(productName, version, kind) {
  return `${productName}_${version}_x64-${kind}.exe`.replace(/\s+/g, ".");
}

/**
 * @param {string} version
 * @param {string} asset
 */
function assetUrl(version, asset) {
  return `https://github.com/${REPO}/releases/download/v${version}/${encodeURIComponent(asset)}`;
}

/**
 * 用 tauri signer 簽章，輸出 <file>.sig；簽章綁定 version（trusted comment 的 version:），和 tauri build 簽安裝檔相同。
 * 私鑰來源同 tauri build：TAURI_SIGNING_PRIVATE_KEY（路徑或內容）或預設路徑。
 * @param {string} file
 * @param {string} version
 * @returns {string} .sig 內容
 */
function signFile(file, version) {
  const envKey = process.env.TAURI_SIGNING_PRIVATE_KEY;
  const { TAURI_SIGNING_PRIVATE_KEY: _key, TAURI_SIGNING_PRIVATE_KEY_PATH: _keyPath, ...env } = process.env;
  // signer 只把 TAURI_SIGNING_PRIVATE_KEY 當內容，路徑要改走 TAURI_SIGNING_PRIVATE_KEY_PATH；內容走環境變數，不放進 argv。
  if (envKey && !existsSync(envKey)) env.TAURI_SIGNING_PRIVATE_KEY = envKey;
  else env.TAURI_SIGNING_PRIVATE_KEY_PATH = envKey || DEFAULT_KEY;
  // 沒設密碼時給空字串，否則 signer 會停下來問密碼。
  env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD = process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "";
  const sigPath = `${file}.sig`;
  rmSync(sigPath, { force: true });
  const result = spawnSync(process.execPath, [TAURI_CLI, "signer", "sign", "--app-version", version, file], {
    cwd: ROOT,
    env,
    encoding: "utf8",
  });
  if (result.error) fail(`無法執行 tauri signer：${result.error.message}`);
  if (result.status !== 0 || !existsSync(sigPath)) {
    fail(`tauri signer sign 失敗（exit ${result.status}）：${`${result.stderr}`.trim() || `${result.stdout}`.trim()}`);
  }
  const signature = readFileSync(sigPath, "utf8").trim();
  writeFileSync(sigPath, signature);
  return signature;
}

/**
 * 把 tauri build 編出的主程式複製成免安裝 exe asset，簽章並用 pubkey 驗證（含綁定的版本）。
 * @param {string} outDir
 * @param {{ productName: string; pubkey: string }} conf
 * @param {string} version
 * @returns {Promise<{ asset: string; signature: string }>}
 */
async function packagePortable(outDir, conf, version) {
  if (!existsSync(PORTABLE_EXE)) fail(`找不到主程式 ${PORTABLE_EXE}`);
  const asset = assetName(conf.productName, version, "portable");
  const file = join(outDir, asset);
  copyFileSync(PORTABLE_EXE, file);
  const signature = signFile(file, version);
  checkSignedVersion(await verifySignature(file, signature, conf.pubkey), version);
  return { asset, signature };
}

/**
 * @param {string} version
 * @param {string} notes
 * @param {{ asset: string; signature: string }} setup
 * @param {{ asset: string; signature: string }} portable
 */
function releaseManifest(version, notes, setup, portable) {
  return {
    version,
    notes,
    pub_date: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms: {
      [PLATFORM_SETUP]: { signature: setup.signature, url: assetUrl(version, setup.asset) },
      [PLATFORM_PORTABLE]: { signature: portable.signature, url: assetUrl(version, portable.asset) },
    },
  };
}

/** @returns {{ productName: string; version: string; pubkey: string }} */
function readTauriConf() {
  const conf = JSON.parse(readFileSync(TAURI_CONF, "utf8"));
  const pubkey = conf.plugins?.updater?.pubkey;
  if (typeof conf.productName !== "string" || typeof conf.version !== "string" || typeof pubkey !== "string") {
    fail("tauri.conf.json 缺 productName、version 或 plugins.updater.pubkey");
  }
  return { productName: conf.productName, version: conf.version, pubkey };
}

/**
 * @param {{ version: string | null; notes: string | null; dryRun: boolean }} opts
 */
async function build(opts) {
  const conf = readTauriConf();
  const current = conf.version;
  let version = current;
  if (opts.dryRun) {
    versionEdits(current, current); // 只核對各檔版本一致
    if (gitStatus()) console.warn("release: 乾跑——工作樹有未提交的變更，這次的產物不要拿去發布。");
  } else {
    version = opts.version ?? fail("缺 <version>");
    parseSemver(version);
    if (compareSemver(version, current) <= 0) fail(`版本 ${version} 要大於目前的 ${current}`);
    if (gitStatus()) fail("工作樹不乾淨：先 commit 或 stash 再發布");
  }

  // 能失敗的檢查都在改版本之前做完，失敗時不會留下改了一半的版本號。
  const envKey = process.env.TAURI_SIGNING_PRIVATE_KEY;
  if (!envKey && !existsSync(DEFAULT_KEY)) fail(`找不到私鑰 ${DEFAULT_KEY}（或用環境變數 TAURI_SIGNING_PRIVATE_KEY 指定）`);
  const notes = opts.notes === null ? "" : readFileSync(resolve(opts.notes), "utf8").replace(/\r\n/g, "\n").trim();
  // 安裝檔附的第三方授權聲明要與目前的依賴一致（docs/release.md「授權與第三方聲明」）；乾跑也查。
  const notices = spawnSync(process.execPath, [join(ROOT, "scripts", "notices.mjs"), "--check"], { cwd: ROOT, stdio: "inherit" });
  if (notices.error) fail(`無法執行 scripts/notices.mjs：${notices.error.message}`);
  if (notices.status !== 0) fail("第三方授權聲明檢查未通過，依上方說明修正後再打包");
  if (!opts.dryRun) {
    for (const { path, text } of versionEdits(current, version)) writeFileSync(path, text);
    console.log(`release: 版本 ${current} → ${version}（${VERSION_SITES.map((s) => s.file).join("、")}）`);
  }

  // 只印來源，不印金鑰內容；環境變數可能直接放金鑰內容而不是路徑。
  console.log(
    envKey
      ? existsSync(envKey)
        ? `release: 私鑰 ${envKey}（TAURI_SIGNING_PRIVATE_KEY）`
        : "release: 私鑰取自 TAURI_SIGNING_PRIVATE_KEY 的內容"
      : `release: 私鑰 ${DEFAULT_KEY}`,
  );

  const installer = join(NSIS_DIR, `${conf.productName}_${version}_x64-setup.exe`);
  // 先清掉同名舊產物：打包沒產出新的 .sig 時不會誤拿上一次的。
  rmSync(installer, { force: true });
  rmSync(`${installer}.sig`, { force: true });
  const result = spawnSync(process.execPath, [TAURI_CLI, "build"], {
    cwd: ROOT,
    stdio: "inherit",
    env: {
      ...process.env,
      TAURI_SIGNING_PRIVATE_KEY: envKey || DEFAULT_KEY,
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "",
    },
  });
  if (result.error) fail(`無法執行 tauri build：${result.error.message}`);
  if (result.status !== 0) fail(`tauri build 失敗（exit ${result.status}）`);
  if (!existsSync(installer) || !existsSync(`${installer}.sig`)) fail(`找不到打包產物 ${installer}（及 .sig）`);

  const signature = readFileSync(`${installer}.sig`, "utf8").trim();
  checkSignedVersion(await verifySignature(installer, signature, conf.pubkey), version);

  const outDir = join(ROOT, "release", `v${version}`);
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  const setup = { asset: assetName(conf.productName, version, "setup"), signature };
  copyFileSync(installer, join(outDir, setup.asset));
  writeFileSync(join(outDir, `${setup.asset}.sig`), signature);
  const portable = await packagePortable(outDir, conf, version);
  for (const name of NOTICE_FILES) copyFileSync(join(ROOT, name), join(outDir, name));
  const manifest = releaseManifest(version, notes, setup, portable);
  writeFileSync(join(outDir, MANIFEST), `${JSON.stringify(manifest, null, 2)}\n`);
  writeFileSync(join(outDir, NOTES_FILE), notes ? `${notes}\n` : "");

  const rel = relative(ROOT, outDir).replaceAll("\\", "/");
  const files = [setup.asset, `${setup.asset}.sig`, portable.asset, `${portable.asset}.sig`, MANIFEST, ...NOTICE_FILES, NOTES_FILE];
  console.log(`\nrelease: 安裝檔與免安裝 exe 的簽章都已用 tauri.conf.json 的 pubkey 驗證通過；產物在 ${rel}/：${files.join("、")}`);
  if (opts.dryRun) {
    console.log("release: 乾跑完成（版本未變更）。");
    return;
  }
  const tag = `v${version}`;
  console.log(`
下一步（細節見 docs/release.md「發布步驟」）：
  1. git add ${VERSION_SITES.map((s) => s.file).join(" ")}
     git commit -m "chore: release ${tag}"
  2. git tag -a ${tag} -m "XIV Launch Manager ${tag}"
  3. 把 commit 與 ${tag} 推上開發 repo
  4. node scripts/export-public.mjs ${version}，再照它印出的指令推送公開 repo
  5. node scripts/release.mjs ${version} --publish`);
}

/**
 * @param {string} version
 */
async function publish(version) {
  parseSemver(version);
  const conf = readTauriConf();
  const outDir = join(ROOT, "release", `v${version}`);
  const manifestPath = join(outDir, MANIFEST);
  if (!existsSync(manifestPath)) fail(`找不到 ${manifestPath}：先執行 node scripts/release.mjs ${version}`);
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  if (manifest.version !== version) fail(`${MANIFEST} 的版本 ${manifest.version} 不是 ${version}`);
  const setup = assetName(conf.productName, version, "setup");
  const portable = assetName(conf.productName, version, "portable");

  // 允許清單：只上傳這八個檔，目錄裡多一個或少一個都不發。
  const allowed = [setup, `${setup}.sig`, portable, `${portable}.sig`, MANIFEST, ...NOTICE_FILES];
  const present = readdirSync(outDir).filter((name) => name !== NOTES_FILE).sort();
  const expected = [...allowed].sort();
  if (present.length !== expected.length || present.some((name, i) => name !== expected[i])) {
    fail(`release/v${version}/ 的檔案（${present.join("、")}）與允許清單（${expected.join("、")}）不符`);
  }
  for (const [platform, asset] of [
    [PLATFORM_SETUP, setup],
    [PLATFORM_PORTABLE, portable],
  ]) {
    const entry = manifest.platforms?.[platform];
    if (typeof entry?.url !== "string" || typeof entry?.signature !== "string") fail(`${MANIFEST} 的 ${platform} 欄位不對`);
    if (entry.url !== assetUrl(version, asset)) fail(`${MANIFEST} 的 ${platform} url 不是預期的 ${assetUrl(version, asset)}`);
    const signature = readFileSync(join(outDir, `${asset}.sig`), "utf8").trim();
    if (signature !== entry.signature) fail(`${MANIFEST} 的 ${platform} signature 與 ${asset}.sig 不同`);
    checkSignedVersion(await verifySignature(join(outDir, asset), signature, conf.pubkey), version);
  }

  const tag = `v${version}`;
  // --verify-tag：tag 還沒推上 GitHub 就失敗，不讓 gh 從預設分支自動建 tag。
  const result = spawnSync(
    "gh",
    ["release", "create", tag, ...allowed, "--repo", REPO, "--title", tag, "--notes-file", NOTES_FILE, "--verify-tag"],
    { cwd: outDir, stdio: "inherit" },
  );
  if (result.error) fail(`無法執行 gh：${result.error.message}`);
  if (result.status !== 0) fail(`gh release create 失敗（exit ${result.status}）`);
  console.log(`release: 已發布 ${tag}；已安裝的版本下次啟動會從 latest.json 看到更新。`);
}

const opts = parseArgs(process.argv.slice(2));
if (opts.publish) await publish(opts.version ?? fail("缺 <version>"));
else await build(opts);
