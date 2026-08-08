import { defineConfig } from 'vitest/config';

// Vitest config lives in its own file, deliberately NOT merged into
// `vite.config.ts`.
//
// Why separate: `vite.config.ts` is the production build path (it feeds
// `vite build` -> `app/ui/`, which `regress/r4_app.sh` serves verbatim).
// Importing `vitest/config` there would make the shippable build fail the day
// vitest is removed or bumped incompatibly. The build must not depend on the
// test runner. Vitest picks this file up ahead of `vite.config.ts` on its own.
//
// Why vitest rather than node:test + tsx:
//   1. `i18n/index.ts` reads `import.meta.env.DEV` inside `t()`, and `t()` is
//      on the hot path of `transportStops.ts`. Vitest supplies `import.meta.env`
//      natively; bare node:test throws "Cannot read properties of undefined"
//      there unless we hand-shim a Vite global into the test bootstrap.
//   2. TypeScript transpiles for free through the esbuild that vite already
//      vendors -- no ts-node/tsx/loader flags to keep in sync with tsconfig.
//
// `environment` stays at the default 'node': every module under test here is a
// pure function with zero DOM access (verified -- `document` appears in
// `i18n/index.ts` only inside `setLocale()`, which no test calls). That keeps
// jsdom/happy-dom out of the dependency tree entirely.
export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    // No `globals: true`: each test file imports describe/it/expect explicitly,
    // so `tsconfig.app.json`'s narrow `"types": ["vite/client"]` keeps working
    // without an ambient-types entry that would apply to app code too.
    globals: false,
  },
});
