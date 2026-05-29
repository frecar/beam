/**
 * Public entry point for the shared E2E smoke foundation.
 *
 * This directory is vendored (copied, not symlinked) from a shared
 * smoke-test foundation. Refresh by re-copying at vendor-bump time.
 * See web/e2e/README.md.
 */

export { ALL_BROWSER_PROJECTS, DEFAULT_BROWSER_PROJECTS, defineSmokeConfig } from './config';
export type { SmokeConfigOverrides } from './config';

export {
  healthcheck200,
  loginPasses,
  matchesAllowlist,
  recordConsoleErrors,
  recordNetworkErrors,
  trimTrailingSlash,
} from './assertions';
export type {
  ConsoleErrorAssertionOptions,
  ConsoleRecorder,
  HealthcheckOptions,
  LoginAssertionOptions,
  NetworkErrorAssertionOptions,
  NetworkRecorder,
} from './assertions';
