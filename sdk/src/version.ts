/**
 * The version of this SDK, as a value the bundled runner can read about ITSELF.
 *
 * A constant rather than an import of `package.json`: `tsconfig.build.json` sets `rootDir: "src"`,
 * so reaching one directory up breaks the npm build, and the runner ships as a single bundled
 * `runner-cli.js` with no `package.json` beside it (`/usr/share/punktfunk-scripting/`), so there is
 * nothing to read at runtime either. Inlining it at build time is the only form that survives both.
 *
 * `version.test.ts` fails if this and `package.json` disagree, so the duplication cannot rot.
 */
export const SDK_VERSION = "0.3.0";
