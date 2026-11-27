#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_CARGO_TOML = path.resolve(SCRIPT_DIR, '..', '..', 'Cargo.toml');
const DEFAULT_OUT_DIR = path.resolve(SCRIPT_DIR, 'platforms');

const SEMVER_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const WORKSPACE_PACKAGE_HEADER_RE = /^\s*\[workspace\.package\]\s*(?:#.*)?$/;
const ANY_SECTION_HEADER_RE = /^\s*\[/;
const VERSION_LINE_RE = /^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?$/;

export const PLATFORMS = Object.freeze([
  Object.freeze({
    dir: 'darwin-arm64',
    name: '@agy-companion/darwin-arm64',
    os: ['darwin'],
    cpu: ['arm64'],
    label: 'macOS arm64 (Apple Silicon)',
  }),
  Object.freeze({
    dir: 'darwin-x64',
    name: '@agy-companion/darwin-x64',
    os: ['darwin'],
    cpu: ['x64'],
    label: 'macOS x64 (Intel)',
  }),
  Object.freeze({
    dir: 'linux-x64',
    name: '@agy-companion/linux-x64',
    os: ['linux'],
    cpu: ['x64'],
    label: 'Linux x64',
  }),
  Object.freeze({
    dir: 'linux-arm64',
    name: '@agy-companion/linux-arm64',
    os: ['linux'],
    cpu: ['arm64'],
    label: 'Linux arm64',
  }),
  Object.freeze({
    dir: 'win32-x64',
    name: '@agy-companion/win32-x64',
    os: ['win32'],
    cpu: ['x64'],
    label: 'Windows x64',
  }),
]);

export function parseWorkspaceVersion(cargoTomlContent) {
  if (typeof cargoTomlContent !== 'string') {
    throw new Error('Cargo.toml content must be a string.');
  }

  let inWorkspacePackage = false;
  for (const rawLine of cargoTomlContent.split(/\r?\n/)) {
    if (WORKSPACE_PACKAGE_HEADER_RE.test(rawLine)) {
      inWorkspacePackage = true;
      continue;
    }
    if (ANY_SECTION_HEADER_RE.test(rawLine)) {
      inWorkspacePackage = false;
      continue;
    }
    if (!inWorkspacePackage) {
      continue;
    }
    const match = VERSION_LINE_RE.exec(rawLine);
    if (match) {
      const version = match[1];
      if (!SEMVER_RE.test(version) || version.includes('..')) {
        throw new Error(`Invalid [workspace.package] version in Cargo.toml: "${version}".`);
      }
      return version;
    }
  }

  throw new Error('Could not find [workspace.package] version in Cargo.toml.');
}

export function buildPlatformManifest(platform, version) {
  return {
    name: platform.name,
    version,
    description: `Prebuilt agy-companion native binary for ${platform.label}.`,
    license: 'Apache-2.0',
    repository: {
      type: 'git',
      url: 'git+https://github.com/Jacksunwei/agy-companion.git',
      directory: 'packages/npm',
    },
    homepage: 'https://github.com/Jacksunwei/agy-companion#readme',
    bugs: {
      url: 'https://github.com/Jacksunwei/agy-companion/issues',
    },
    os: [...platform.os],
    cpu: [...platform.cpu],
    files: ['bin/'],
    preferUnplugged: true,
    publishConfig: {
      registry: 'https://registry.npmjs.org/',
      access: 'public',
    },
  };
}

export function buildPlatformReadme(platform) {
  return [
    `# ${platform.name}`,
    '',
    `Prebuilt \`agy-companion\` native binary package for **${platform.label}**.`,
    '',
    'This package is an internal platform-specific optional dependency of [`agy-companion`](https://www.npmjs.com/package/agy-companion) and is installed automatically for your OS and CPU architecture.',
    '',
    'Install the main CLI package instead:',
    '',
    '```bash',
    'npx -y agy-companion@latest install',
    '```',
    '',
    'See the [Antigravity Companion repository](https://github.com/Jacksunwei/agy-companion) for documentation and source code.',
    '',
  ].join('\n');
}

export function generatePlatformPackages({
  cargoTomlPath = DEFAULT_CARGO_TOML,
  outDir = DEFAULT_OUT_DIR,
} = {}) {
  if (typeof outDir !== 'string' || outDir.trim() === '') {
    throw new Error('--out-dir must be a non-empty directory path.');
  }
  const cargoTomlContent = fs.readFileSync(cargoTomlPath, 'utf8');
  const version = parseWorkspaceVersion(cargoTomlContent);

  const written = [];
  for (const platform of PLATFORMS) {
    const pkgDir = path.join(outDir, platform.dir);
    fs.mkdirSync(pkgDir, { recursive: true });
    const manifestPath = path.join(pkgDir, 'package.json');
    const manifest = buildPlatformManifest(platform, version);
    fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, 'utf8');
    const readmePath = path.join(pkgDir, 'README.md');
    fs.writeFileSync(readmePath, buildPlatformReadme(platform), 'utf8');
    written.push({ platform: platform.dir, manifestPath, readmePath, manifest });
  }

  return { version, outDir, written };
}

export function parseArgs(argv = process.argv.slice(2)) {
  let outDir = DEFAULT_OUT_DIR;
  let help = false;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '-h' || arg === '--help') {
      help = true;
    } else if (arg === '--out-dir') {
      if (i + 1 >= argv.length) {
        throw new Error('Missing value for --out-dir.');
      }
      const next = argv[i + 1];
      if (!next || next.trim() === '' || next.startsWith('-')) {
        throw new Error(`Invalid value for --out-dir: "${next ?? ''}".`);
      }
      outDir = path.resolve(next);
      i += 1;
    } else if (arg.startsWith('--out-dir=')) {
      const value = arg.slice('--out-dir='.length);
      if (value.trim() === '') {
        throw new Error('Missing value for --out-dir=.');
      }
      outDir = path.resolve(value);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  return { outDir, help };
}

function usage() {
  return [
    'Usage: generate-platform-pkgs.mjs [--out-dir <dir>] [--help]',
    '',
    'Generate @agy-companion/<os>-<arch>/package.json manifests synced to root Cargo.toml.',
    '',
    'Options:',
    `  --out-dir <dir>   Output directory (default: ${DEFAULT_OUT_DIR})`,
    '  -h, --help        Show this help message and exit',
  ].join('\n');
}

function isDirectCliRun() {
  if (!process.argv[1]) {
    return false;
  }
  try {
    return fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}

if (isDirectCliRun()) {
  try {
    const { outDir, help } = parseArgs();
    if (help) {
      process.stdout.write(`${usage()}\n`);
      process.exit(0);
    }
    const { version, written } = generatePlatformPackages({ outDir });
    for (const entry of written) {
      process.stdout.write(`Generated ${entry.manifest.name}@${version} -> ${entry.manifestPath}\n`);
    }
  } catch (err) {
    const message = err && err.message ? err.message : String(err);
    process.stderr.write(`generate-platform-pkgs.mjs: ${message}\n`);
    process.stderr.write(`${usage()}\n`);
    process.exit(1);
  }
}
