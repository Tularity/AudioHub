#!/usr/bin/env node

import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const REQUIRED_CARGO_ABOUT = "0.9.1";
const ACCEPTED_FRONTEND_LICENSES = new Set(["MIT"]);
const scriptDir = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDir, "..");
const temporary = mkdtempSync(join(tmpdir(), "audiohub-licenses-"));

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? repository,
    encoding: "utf8",
    stdio: options.capture ? "pipe" : "inherit",
    maxBuffer: 32 * 1024 * 1024,
    shell: options.shell ?? false,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (options.capture) {
      process.stderr.write(result.stdout ?? "");
      process.stderr.write(result.stderr ?? "");
    }
    throw new Error(`${command} exited with status ${result.status}`);
  }
  return result.stdout?.trim() ?? "";
}

function frontendRuntimePackages() {
  const frontend = join(repository, "app", "frontend");
  const lockPath = join(frontend, "package-lock.json");
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  if (lock.lockfileVersion !== 3 || typeof lock.packages !== "object") {
    throw new Error("app/frontend/package-lock.json must use npm lockfileVersion 3");
  }
  const root = lock.packages[""];
  if (!root || typeof root.dependencies !== "object") {
    throw new Error("frontend lockfile has no root production dependencies");
  }

  // Resolve the production graph from the lockfile itself. npm records packages
  // used exclusively by dev dependencies as `dev`, and packages present only to
  // satisfy an optional dev peer as `devOptional`; neither is shipped in Vite's
  // production bundle. Traversal is still performed (rather than trusting those
  // flags alone) and the two views must agree, so a malformed lock cannot make a
  // runtime package silently disappear from the notice.
  function resolveDependency(fromPath, name) {
    let parent = fromPath;
    for (;;) {
      const candidate = parent
        ? `${parent}/node_modules/${name}`
        : `node_modules/${name}`;
      if (lock.packages[candidate]) return candidate;
      const marker = parent.lastIndexOf("/node_modules/");
      if (marker < 0) {
        if (!parent) return null;
        parent = "";
      } else {
        parent = parent.slice(0, marker);
      }
    }
  }

  function dependenciesOf(packagePath, packageRecord) {
    const dependencies = [];
    for (const name of Object.keys(packageRecord.dependencies ?? {})) {
      dependencies.push({ name, optional: false });
    }
    for (const name of Object.keys(packageRecord.optionalDependencies ?? {})) {
      if (!dependencies.some((dependency) => dependency.name === name)) {
        dependencies.push({ name, optional: true });
      }
    }
    for (const name of Object.keys(packageRecord.peerDependencies ?? {})) {
      const optional = packageRecord.peerDependenciesMeta?.[name]?.optional === true;
      // An optional peer is a compatibility declaration, not a runtime edge.
      // If the root application actually uses it, it is reached from the root
      // dependencies independently (React is the current example).
      if (optional) continue;
      if (!dependencies.some((dependency) => dependency.name === name)) {
        dependencies.push({ name, optional: false });
      }
    }
    return dependencies.map(({ name, optional }) => {
      const resolved = resolveDependency(packagePath, name);
      if (!resolved && !optional) {
        throw new Error(`${packagePath || "frontend root"} cannot resolve ${name} from package-lock.json`);
      }
      if (resolved && optional &&
          (lock.packages[resolved].dev === true || lock.packages[resolved].devOptional === true)) {
        return null;
      }
      return resolved;
    }).filter(Boolean);
  }

  const pending = Object.keys(root.dependencies).map((name) => {
    const packagePath = resolveDependency("", name);
    if (!packagePath) throw new Error(`frontend root cannot resolve ${name} from package-lock.json`);
    return packagePath;
  });
  const productionPaths = new Set();
  while (pending.length > 0) {
    const packagePath = pending.pop();
    if (productionPaths.has(packagePath)) continue;
    const packageRecord = lock.packages[packagePath];
    if (!packageRecord || packageRecord.dev === true || packageRecord.devOptional === true) {
      throw new Error(`frontend production dependency is marked dev-only: ${packagePath}`);
    }
    productionPaths.add(packagePath);
    pending.push(...dependenciesOf(packagePath, packageRecord));
  }

  const flaggedProductionPaths = Object.entries(lock.packages)
    .filter(([packagePath, packageRecord]) => (
      packagePath.startsWith("node_modules/") &&
      packageRecord.dev !== true &&
      packageRecord.devOptional !== true
    ))
    .map(([packagePath]) => packagePath)
    .sort();
  const traversedProductionPaths = [...productionPaths].sort();
  if (JSON.stringify(flaggedProductionPaths) !== JSON.stringify(traversedProductionPaths)) {
    const onlyFlagged = flaggedProductionPaths.filter((path) => !productionPaths.has(path));
    const onlyTraversed = traversedProductionPaths.filter((path) => !flaggedProductionPaths.includes(path));
    throw new Error(
      `frontend production graph disagrees with npm lock flags; ` +
      `only flagged: ${onlyFlagged.join(", ") || "none"}; ` +
      `only traversed: ${onlyTraversed.join(", ") || "none"}`,
    );
  }

  const nodeModules = join(frontend, "node_modules");
  if (!existsSync(nodeModules)) {
    // npm verifies every downloaded tarball against the integrity hash in the
    // committed lockfile. A full install is intentional: the immediately
    // following frontend build also needs the locked dev tools.
    run("npm", ["ci", "--no-audit", "--no-fund"], {
      cwd: frontend,
      // npm is a .cmd shim on Windows and therefore needs cmd.exe. Every
      // argument here is a fixed literal; no lockfile data enters the shell.
      shell: process.platform === "win32",
    });
  }

  const packages = traversedProductionPaths.map((packagePath) => {
    const locked = lock.packages[packagePath];
    const packageRoot = join(frontend, packagePath);
    let manifest;
    try {
      manifest = JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8"));
    } catch (error) {
      throw new Error(`locked frontend package is not installed: ${packagePath}: ${error.message}`);
    }
    const expectedSuffix = `node_modules/${manifest.name}`;
    if (packagePath !== expectedSuffix && !packagePath.endsWith(`/${expectedSuffix}`)) {
      throw new Error(`installed frontend package name disagrees with lock: ${packagePath}`);
    }
    if (manifest.version !== locked.version) {
      throw new Error(
        `installed frontend package version disagrees with lock: ` +
        `${manifest.name} is ${manifest.version}, expected ${locked.version}`,
      );
    }
    if (typeof locked.license !== "string" || locked.license.trim() === "") {
      throw new Error(`frontend package has no locked SPDX license: ${manifest.name} ${locked.version}`);
    }
    if (!ACCEPTED_FRONTEND_LICENSES.has(locked.license)) {
      throw new Error(
        `frontend package uses an unreviewed license: ` +
        `${manifest.name} ${locked.version} is ${locked.license}`,
      );
    }
    if (manifest.license !== locked.license) {
      throw new Error(
        `installed frontend package license disagrees with lock: ` +
        `${manifest.name} ${locked.version} is ${manifest.license ?? "missing"}, expected ${locked.license}`,
      );
    }
    const legalFiles = readdirSync(packageRoot, { withFileTypes: true })
      .filter((entry) => (
        entry.isFile() &&
        /^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT|AUTHORS)(?:[._-]|$)/i.test(entry.name)
      ))
      .map((entry) => entry.name)
      .sort();
    if (!legalFiles.some((filename) => /^(LICENSE|LICENCE|COPYING)(?:[._-]|$)/i.test(filename))) {
      throw new Error(`frontend package has no installed license text: ${manifest.name} ${locked.version}`);
    }
    return {
      name: manifest.name,
      version: manifest.version,
      license: locked.license,
      files: legalFiles.map((filename) => ({
        filename,
        text: normalizeNewlines(readFileSync(join(packageRoot, filename), "utf8")),
      })),
    };
  }).sort((left, right) => {
    const leftKey = `${left.name}\u0000${left.version}`;
    const rightKey = `${right.name}\u0000${right.version}`;
    return leftKey < rightKey ? -1 : leftKey > rightKey ? 1 : 0;
  });

  for (const required of [
    "react",
    "react-dom",
    "scheduler",
    "use-sync-external-store",
    "zustand",
  ]) {
    if (!packages.some((packageRecord) => packageRecord.name === required)) {
      throw new Error(`frontend license report is missing required runtime package: ${required}`);
    }
  }
  return packages;
}

function renderFrontendLicenses(packages) {
  const inventory = packages.map((packageRecord) => `
      <tr>
        <td>${escapeHtml(packageRecord.name)}</td>
        <td>${escapeHtml(packageRecord.version)}</td>
        <td><code>${escapeHtml(packageRecord.license)}</code></td>
      </tr>`).join("");
  const texts = packages.flatMap((packageRecord) => packageRecord.files.map((file) => `
    <article class="license-block">
      <h3>${escapeHtml(packageRecord.name)} ${escapeHtml(packageRecord.version)} — ${escapeHtml(file.filename)}</h3>
      <pre>${escapeHtml(file.text)}</pre>
    </article>`)).join("");
  return `
    <h3>Resolved package inventory</h3>
    <table>
      <thead>
        <tr><th>Package</th><th>Version</th><th>Resolved license</th></tr>
      </thead>
      <tbody>${inventory}
      </tbody>
    </table>
    <h3>License texts and notices</h3>${texts}`;
}

function escapeHtml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function normalizeNewlines(value) {
  return value
    .replaceAll("\r\n", "\n")
    .replaceAll("\r", "\n")
    // Upstream license texts occasionally carry insignificant spaces at EOL.
    // Keep the legal text intact while making the committed report pass the
    // repository's whitespace checks on every platform.
    .replace(/[ \t]+$/gm, "");
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function packageIsInFragment(fragment, cargoPackage) {
  const name = escapeRegex(escapeHtml(cargoPackage.name));
  const version = escapeRegex(escapeHtml(cargoPackage.version));
  return new RegExp(`>${name}(?:</a>)?</td>\\s*<td>${version}</td>`).test(fragment);
}

function collectAdditionalNotices(fragment, manifest) {
  const metadata = JSON.parse(run("cargo", [
    "metadata",
    "--format-version",
    "1",
    "--frozen",
    "--manifest-path",
    manifest,
  ], { capture: true }));

  const notices = [];
  for (const cargoPackage of metadata.packages) {
    if (!packageIsInFragment(fragment, cargoPackage)) continue;
    const crateRoot = dirname(cargoPackage.manifest_path);
    for (const entry of readdirSync(crateRoot, { withFileTypes: true })) {
      if (!entry.isFile() || !/^(NOTICE|COPYRIGHT|AUTHORS)(?:[._-]|$)/i.test(entry.name)) {
        continue;
      }
      notices.push({
        package: cargoPackage.name,
        version: cargoPackage.version,
        filename: entry.name,
        text: normalizeNewlines(readFileSync(join(crateRoot, entry.name), "utf8")),
      });
    }
  }
  return notices;
}

function renderAdditionalNotices(notices) {
  return notices.map((notice) => `
    <article class="license-block">
      <h3>${escapeHtml(notice.package)} ${escapeHtml(notice.version)} — ${escapeHtml(notice.filename)}</h3>
      <pre>${escapeHtml(notice.text)}</pre>
    </article>`).join("");
}

function cargoAbout(config, args, output) {
  run("cargo", [
    "about",
    "generate",
    "--frozen",
    "--fail",
    "--config",
    join(repository, config),
    "--output-file",
    output,
    ...args,
    join(repository, "about.hbs"),
  ]);
}

try {
  const aboutVersion = run("cargo", ["about", "--version"], { capture: true });
  if (aboutVersion !== `cargo-about ${REQUIRED_CARGO_ABOUT}`) {
    throw new Error(
      `cargo-about ${REQUIRED_CARGO_ABOUT} is required; found ${aboutVersion || "none"}. ` +
      `Install it with: cargo install --locked cargo-about --version ${REQUIRED_CARGO_ABOUT} --features cli`,
    );
  }

  const frontendPackages = frontendRuntimePackages();
  const frontend = renderFrontendLicenses(frontendPackages);

  // Fetch only checksum-locked crates first. The cargo-about passes themselves
  // stay frozen/offline, so report contents cannot depend on a mutable license
  // service. Cargo package contents are verified by the two Cargo.lock files.
  for (const manifest of ["Cargo.toml", "app/src-tauri/Cargo.toml"]) {
    for (const target of ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"]) {
      run("cargo", ["fetch", "--locked", "--manifest-path", manifest, "--target", target]);
    }
  }

  const coreFragment = join(temporary, "core.html");
  const appFragment = join(temporary, "app.html");
  cargoAbout(
    "about.toml",
    ["--workspace", "--manifest-path", join(repository, "Cargo.toml")],
    coreFragment,
  );
  cargoAbout(
    "app/src-tauri/about.toml",
    ["--manifest-path", join(repository, "app/src-tauri/Cargo.toml")],
    appFragment,
  );

  const core = normalizeNewlines(readFileSync(coreFragment, "utf8"));
  const app = normalizeNewlines(readFileSync(appFragment, "utf8"));
  const requiredCoreCrates = [
    "alac",
    "base64",
    "curve25519-dalek",
    "ed25519-dalek",
    "rsa",
    "subtle",
    "symphonia",
  ];
  for (const crate of requiredCoreCrates) {
    if (!core.includes(`>${crate}<`) && !core.includes(`${crate} `)) {
      throw new Error(`core license report is missing required crate: ${crate}`);
    }
  }
  for (const crate of ["curve25519-dalek", "ed25519-dalek", "subtle"]) {
    if (!core.includes(`<li>${crate} `)) {
      throw new Error(`core license texts do not attribute required BSD crate: ${crate}`);
    }
  }
  for (const crate of ["tauri", "webview2-com", "wry"]) {
    if (!app.includes(`>${crate}<`) && !app.includes(`${crate} `)) {
      throw new Error(`desktop license report is missing required crate: ${crate}`);
    }
  }
  if (!core.includes("Redistribution and use in source and binary forms")) {
    throw new Error("core license report is missing the BSD redistribution terms");
  }
  if (!core.includes("Mozilla Public License Version 2.0")) {
    throw new Error("core license report is missing the MPL-2.0 terms");
  }
  if (core.includes(">Unknown<") || app.includes(">Unknown<")) {
    throw new Error("license report contains an unresolved package license");
  }

  const additionalNotices = [
    ...collectAdditionalNotices(core, "Cargo.toml"),
    ...collectAdditionalNotices(app, "app/src-tauri/Cargo.toml"),
  ];
  const uniqueNotices = [...new Map(additionalNotices.map((notice) => [
    `${notice.package}\u0000${notice.version}\u0000${notice.filename}`,
    notice,
  ])).values()].sort((left, right) => {
    const leftKey = `${left.package}\u0000${left.version}\u0000${left.filename}`;
    const rightKey = `${right.package}\u0000${right.version}\u0000${right.filename}`;
    return leftKey < rightKey ? -1 : leftKey > rightKey ? 1 : 0;
  });
  const template = normalizeNewlines(
    readFileSync(join(repository, "third-party-licenses.template.html"), "utf8"),
  );
  const output = normalizeNewlines(
    template
      .replace("<!-- AUDIOHUB_CORE_LICENSES -->", core)
      .replace("<!-- AUDIOHUB_APP_LICENSES -->", app)
      .replace("<!-- AUDIOHUB_FRONTEND_LICENSES -->", frontend)
      .replace("<!-- AUDIOHUB_ADDITIONAL_NOTICES -->", renderAdditionalNotices(uniqueNotices)),
  );
  if (output.includes("<!-- AUDIOHUB_")) {
    throw new Error("license report template contains an unresolved placeholder");
  }
  if (output.includes(repository)) {
    throw new Error("license report leaked the local repository path");
  }

  const destination = join(repository, "THIRD-PARTY-LICENSES.html");
  writeFileSync(destination, output, "utf8");
  process.stdout.write(`generated ${destination}\n`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
