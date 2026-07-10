/**
 * Public entry point for the shared E2E smoke foundation.
 *
 * This directory is vendored (copied, not symlinked) from a shared
 * smoke-test foundation. Refresh by re-copying at vendor-bump time.
 * See web/e2e/README.md.
 */

export {
  ALL_BROWSER_PROJECTS,
  CHROMIUM_UNSAFE_PORTS,
  DEFAULT_BROWSER_PROJECTS,
  defineSmokeConfig,
  derivePreviewPort,
  GATEWAY_CSP_PORT_BASE,
  LOCAL_GATEWAY_CSP_PORT,
  LOCAL_PREVIEW_PORT,
  PORT_BAND_SIZE,
  PREVIEW_PORT_BASE,
  resolveGatewayCspPort,
  resolvePreviewPort,
} from './config';
export type {
  SmokeConfigOverrides,
  SmokeManualServerOverrides,
  SmokePreviewOverrides,
  SmokePreviewServerOverrides,
} from './config';

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
