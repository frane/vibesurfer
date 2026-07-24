#!/usr/bin/env node
// Launcher for the vibesurfer (`vs`) native binary.
//
// vibesurfer is a Rust binary, one per platform. This thin package
// lets you run it with zero install:
//
//   npx vibesurfer session-open
//
// On first run it downloads the prebuilt binary for this platform from
// the matching GitHub release, verifies its SHA-256 against the
// release's checksums.txt, caches it under ~/.vibesurfer/bin/<version>,
// and execs it. Every later run reuses the cache and never touches the
// network. The npm package version pins the release it fetches, so
// `npx vibesurfer@0.1.28` runs exactly that build.

"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");
const crypto = require("crypto");
const { spawnSync } = require("child_process");

const REPO = "frane/vibesurfer";
const VERSION = require("../package.json").version;

// platform+arch -> { triple, ext }. Only targets we publish binaries
// for; anything else falls through to a build-from-source message.
function target() {
  const key = `${process.platform}-${process.arch}`;
  const map = {
    "darwin-arm64": { triple: "aarch64-apple-darwin", ext: "tar.gz" },
    "darwin-x64": { triple: "x86_64-apple-darwin", ext: "tar.gz" },
    "linux-x64": { triple: "x86_64-unknown-linux-gnu", ext: "tar.gz" },
    "win32-x64": { triple: "x86_64-pc-windows-msvc", ext: "zip" },
  };
  return map[key] || null;
}

function binName() {
  return process.platform === "win32" ? "vs.exe" : "vs";
}

function cacheDir() {
  const home = os.homedir() || os.tmpdir();
  return path.join(home, ".vibesurfer", "bin", VERSION);
}

async function fetchBuffer(url) {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`GET ${url} -> ${res.status}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

function sha256(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

// Parse `<sha>  <asset>` lines from checksums.txt into a map.
function parseChecksums(text) {
  const out = {};
  for (const line of text.split(/\r?\n/)) {
    const m = line.trim().match(/^([0-9a-f]{64})\s+(\S+)$/i);
    if (m) out[m[2]] = m[1].toLowerCase();
  }
  return out;
}

async function download(dest) {
  const t = target();
  if (!t) {
    throw new Error(
      `no prebuilt vibesurfer binary for ${process.platform}/${process.arch}. ` +
        `Install from source: cargo install vibesurfer (needs Rust), ` +
        `or see https://github.com/${REPO}#install`
    );
  }
  const tag = `v${VERSION}`;
  const asset = `vs-${tag}-${t.triple}.${t.ext}`;
  const base = `https://github.com/${REPO}/releases/download/${tag}`;

  process.stderr.write(`vibesurfer: fetching ${asset} (first run)\n`);
  const [archive, sumsText] = await Promise.all([
    fetchBuffer(`${base}/${asset}`),
    fetchBuffer(`${base}/checksums.txt`).then((b) => b.toString("utf8")),
  ]);

  const want = parseChecksums(sumsText)[asset];
  if (!want) throw new Error(`${asset} not listed in checksums.txt`);
  const got = sha256(archive);
  if (got !== want) {
    throw new Error(`checksum mismatch for ${asset}: got ${got}, want ${want}`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "vs-dl-"));
  try {
    const archivePath = path.join(tmp, asset);
    fs.writeFileSync(archivePath, archive);
    // bsdtar (macOS/Windows) extracts both .tar.gz and .zip; GNU tar
    // (Linux) handles our .tar.gz. So `tar -xf` covers every platform's
    // own asset format.
    const r = spawnSync("tar", ["-xf", archivePath, "-C", tmp], {
      stdio: "inherit",
    });
    if (r.status !== 0) {
      throw new Error(`extract failed (tar exit ${r.status ?? r.signal})`);
    }
    const extracted = path.join(tmp, binName());
    if (!fs.existsSync(extracted)) {
      throw new Error(`binary ${binName()} not found in ${asset}`);
    }
    fs.mkdirSync(cacheDir(), { recursive: true });
    fs.copyFileSync(extracted, dest);
    if (process.platform !== "win32") fs.chmodSync(dest, 0o755);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

async function ensureBinary() {
  const dest = path.join(cacheDir(), binName());
  if (fs.existsSync(dest)) return dest;
  await download(dest);
  return dest;
}

async function main() {
  let bin;
  try {
    bin = await ensureBinary();
  } catch (e) {
    process.stderr.write(`vibesurfer: ${e.message}\n`);
    process.exit(1);
  }
  const child = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (child.error) {
    process.stderr.write(`vibesurfer: ${child.error.message}\n`);
    process.exit(1);
  }
  process.exit(child.status === null ? 1 : child.status);
}

main();
