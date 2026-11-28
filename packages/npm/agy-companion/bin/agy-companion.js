#!/usr/bin/env node
'use strict';

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');

const SUPPORTED_TARGETS = new Map([
  [
    'darwin:arm64',
    Object.freeze({
      pkg: '@agy-companion/darwin-arm64',
      specifier: '@agy-companion/darwin-arm64/bin/agy-companion',
    }),
  ],
  [
    'darwin:x64',
    Object.freeze({
      pkg: '@agy-companion/darwin-x64',
      specifier: '@agy-companion/darwin-x64/bin/agy-companion',
    }),
  ],
  [
    'linux:x64',
    Object.freeze({
      pkg: '@agy-companion/linux-x64',
      specifier: '@agy-companion/linux-x64/bin/agy-companion',
    }),
  ],
  [
    'linux:arm64',
    Object.freeze({
      pkg: '@agy-companion/linux-arm64',
      specifier: '@agy-companion/linux-arm64/bin/agy-companion',
    }),
  ],
  [
    'win32:x64',
    Object.freeze({
      pkg: '@agy-companion/win32-x64',
      specifier: '@agy-companion/win32-x64/bin/agy-companion.exe',
    }),
  ],
]);

const SUPPORTED_LIST = 'darwin-arm64, darwin-x64, linux-x64, linux-arm64, win32-x64';

function resolveTarget(platform = process.platform, arch = process.arch) {
  if (typeof platform !== 'string' || typeof arch !== 'string') {
    return null;
  }
  return SUPPORTED_TARGETS.get(`${platform}:${arch}`) || null;
}

function resolveBinaryPath({
  platform = process.platform,
  arch = process.arch,
  resolve = require.resolve,
  statSync = fs.statSync,
} = {}) {
  const target = resolveTarget(platform, arch);
  if (!target) {
    const err = new Error(
      `Unsupported platform/architecture: ${String(platform)}-${String(arch)}. ` +
        `Supported targets: ${SUPPORTED_LIST}.`
    );
    err.code = 'UNSUPPORTED_PLATFORM';
    throw err;
  }

  let binaryPath;
  try {
    binaryPath = resolve(target.specifier);
    const stats = statSync(binaryPath);
    if (!stats || !stats.isFile()) {
      throw new Error(`Resolved path is not a regular file: ${binaryPath}`);
    }
  } catch (cause) {
    const err = new Error(
      `Unable to locate prebuilt binary package ${target.pkg} (${target.specifier}). ` +
        `Reinstall agy-companion without --omit=optional or --no-optional so platform ` +
        `optionalDependencies are installed.`
    );
    err.code = 'MISSING_BINARY';
    err.cause = cause;
    throw err;
  }

  return binaryPath;
}

function isNpxInvocation(env = process.env, dir = __dirname) {
  if (env && env.npm_command === 'exec') {
    return true;
  }
  if (typeof dir === 'string' && dir.split(/[\\/]+/).includes('_npx')) {
    return true;
  }
  return false;
}

function buildChildEnv(env = process.env, dir = __dirname) {
  if (env && !env.AGY_COMPANION_INSTALL_CHANNEL && isNpxInvocation(env, dir)) {
    return { ...env, AGY_COMPANION_INSTALL_CHANNEL: 'npx' };
  }
  return env;
}

function run(argv = process.argv.slice(2)) {
  let binaryPath;
  try {
    binaryPath = resolveBinaryPath();
  } catch (err) {
    process.stderr.write(`agy-companion: ${err.message}\n`);
    process.exit(1);
  }

  try {
    execFileSync(binaryPath, argv, {
      stdio: 'inherit',
      shell: false,
      env: buildChildEnv(),
    });
  } catch (err) {
    if (err && typeof err.status === 'number') {
      process.exit(err.status);
    }
    if (err && typeof err.signal === 'string') {
      const sigNum =
        os.constants && os.constants.signals ? os.constants.signals[err.signal] : undefined;
      try {
        process.kill(process.pid, err.signal);
      } catch (_) {
        // Fall through to deterministic non-zero exit code below.
      }
      process.exit(typeof sigNum === 'number' ? 128 + sigNum : 1);
    }
    const detail = err && err.message ? err.message : String(err);
    process.stderr.write(`agy-companion: failed to launch native binary: ${detail}\n`);
    process.exit(1);
  }
}

if (require.main === module) {
  run();
}

module.exports = {
  SUPPORTED_TARGETS,
  resolveTarget,
  resolveBinaryPath,
  isNpxInvocation,
  buildChildEnv,
  run,
};
