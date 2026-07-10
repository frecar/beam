import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import {
  ALL_BROWSER_PROJECTS,
  DEFAULT_BROWSER_PROJECTS,
  defineSmokeConfig,
  derivePreviewPort,
  GATEWAY_CSP_PORT_BASE,
  LOCAL_GATEWAY_CSP_PORT,
  LOCAL_PREVIEW_PORT,
  PREVIEW_PORT_BASE,
  resolveGatewayCspPort,
  resolvePreviewPort,
} from '../config';

describe('defineSmokeConfig', () => {
  const originalCI = process.env.CI;
  const originalRunId = process.env.GITHUB_RUN_ID;

  beforeEach(() => {
    delete process.env.CI;
    delete process.env.GITHUB_RUN_ID;
  });

  afterEach(() => {
    if (originalCI === undefined) {
      delete process.env.CI;
    } else {
      process.env.CI = originalCI;
    }
    if (originalRunId === undefined) {
      delete process.env.GITHUB_RUN_ID;
    } else {
      process.env.GITHUB_RUN_ID = originalRunId;
    }
  });

  it('returns chromium-only projects by default', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.projects).toEqual(DEFAULT_BROWSER_PROJECTS);
    expect(config.projects).toHaveLength(1);
    expect(config.projects?.[0]?.name).toBe('chromium');
  });

  it('exposes ALL_BROWSER_PROJECTS preset with three browsers', () => {
    expect(ALL_BROWSER_PROJECTS).toHaveLength(3);
    const names = ALL_BROWSER_PROJECTS.map((p) => p.name);
    expect(names).toEqual(['chromium', 'firefox', 'webkit']);
  });

  it('honours custom projects override', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      projects: ALL_BROWSER_PROJECTS,
    });
    expect(config.projects).toEqual(ALL_BROWSER_PROJECTS);
  });

  it('applies shared default timeouts', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.timeout).toBe(30_000);
    expect(config.expect?.timeout).toBe(5_000);
    // SMOKE_USE_DEFAULTS keys
    expect(config.use?.actionTimeout).toBe(10_000);
    expect(config.use?.navigationTimeout).toBe(30_000);
    expect(config.use?.screenshot).toBe('only-on-failure');
    expect(config.use?.trace).toBe('retain-on-failure');
    expect(config.use?.video).toBe('off');
  });

  it('overrides timeouts when provided', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      timeout: 60_000,
      expectTimeout: 10_000,
    });
    expect(config.timeout).toBe(60_000);
    expect(config.expect?.timeout).toBe(10_000);
  });

  it('threads baseURL into use', () => {
    const config = defineSmokeConfig({ baseURL: 'http://127.0.0.1:4173' });
    expect(config.use?.baseURL).toBe('http://127.0.0.1:4173');
  });

  it('passes webServer through unchanged', () => {
    const webServer = {
      command: 'npm run preview',
      url: 'http://127.0.0.1:4173',
      timeout: 120_000,
    };
    const config = defineSmokeConfig({
      baseURL: 'http://127.0.0.1:4173',
      webServer,
    });
    expect(config.webServer).toEqual(webServer);
  });

  it('omits webServer when not provided (exactOptionalPropertyTypes)', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.webServer).toBeUndefined();
    expect('webServer' in config).toBe(false);
  });

  it('defaults retries to 0 outside CI', () => {
    delete process.env.CI;
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.retries).toBe(0);
    expect(config.forbidOnly).toBe(false);
  });

  it('defaults retries to 1 in CI and forbidOnly to true', () => {
    process.env.CI = 'true';
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.retries).toBe(1);
    expect(config.forbidOnly).toBe(true);
  });

  it('treats empty CI env var as non-CI', () => {
    process.env.CI = '';
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.retries).toBe(0);
    expect(config.forbidOnly).toBe(false);
  });

  it('honours explicit retries override even outside CI', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      retries: 3,
    });
    expect(config.retries).toBe(3);
  });

  it('defaults workers to 1 (deterministic ordering)', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.workers).toBe(1);
    expect(config.fullyParallel).toBe(false);
  });

  it('honours custom worker count', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      workers: 4,
    });
    expect(config.workers).toBe(4);
  });

  it('uses list reporter outside CI and github reporter in CI', () => {
    delete process.env.CI;
    const local = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(local.reporter).toBeInstanceOf(Array);
    expect((local.reporter as unknown[])[0]).toEqual(['list']);

    process.env.CI = 'true';
    const ci = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect((ci.reporter as unknown[])[0]).toEqual(['github']);
  });

  it('always includes html reporter pointed at playwright-report', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    const reporters = config.reporter as Array<[string, unknown]>;
    const html = reporters.find((r) => r[0] === 'html');
    expect(html).toBeDefined();
    expect(html?.[1]).toEqual({ open: 'never', outputFolder: 'playwright-report' });
  });

  it('appends extra reporters after baseline', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      extraReporters: [['junit', { outputFile: 'junit.xml' }]],
    });
    const reporters = config.reporter as Array<[string, unknown]>;
    expect(reporters).toHaveLength(3);
    expect(reporters[2]).toEqual(['junit', { outputFile: 'junit.xml' }]);
  });

  it('defaults testDir to ./e2e', () => {
    const config = defineSmokeConfig({ baseURL: 'http://localhost:8000' });
    expect(config.testDir).toBe('./e2e');
  });

  it('honours testDir override', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://localhost:8000',
      testDir: './tests/browser',
    });
    expect(config.testDir).toBe('./tests/browser');
  });
});

describe('per-run port derivation', () => {
  const originalRunId = process.env.GITHUB_RUN_ID;

  beforeEach(() => {
    delete process.env.GITHUB_RUN_ID;
  });

  afterEach(() => {
    if (originalRunId === undefined) {
      delete process.env.GITHUB_RUN_ID;
    } else {
      process.env.GITHUB_RUN_ID = originalRunId;
    }
  });

  it('maps run ids into the 10000-10999 preview band', () => {
    expect(derivePreviewPort('0')).toBe(10_000);
    expect(derivePreviewPort('123')).toBe(10_123);
    expect(derivePreviewPort('999')).toBe(10_999);
    expect(derivePreviewPort('16548561341')).toBe(10_341);
  });

  it('remaps the Chromium-unsafe 10080 (amanda) to the adjacent safe slot', () => {
    // 10080 is Chromium-restricted → net::ERR_UNSAFE_PORT.
    expect(derivePreviewPort('80')).toBe(10_081);
    expect(derivePreviewPort('1080')).toBe(10_081);
    // Neighbours are untouched.
    expect(derivePreviewPort('79')).toBe(10_079);
    expect(derivePreviewPort('81')).toBe(10_081);
  });

  it('falls back to the local preview port on an unparsable run id', () => {
    expect(derivePreviewPort('not-a-number')).toBe(LOCAL_PREVIEW_PORT);
    expect(derivePreviewPort('')).toBe(LOCAL_PREVIEW_PORT);
  });

  it('resolvePreviewPort uses GITHUB_RUN_ID in CI and 4173 locally', () => {
    expect(resolvePreviewPort()).toBe(LOCAL_PREVIEW_PORT);
    process.env.GITHUB_RUN_ID = '12345';
    expect(resolvePreviewPort()).toBe(10_345);
  });

  it('resolvePreviewPort accepts an explicit env record', () => {
    expect(resolvePreviewPort({ GITHUB_RUN_ID: '42' })).toBe(10_042);
    expect(resolvePreviewPort({})).toBe(LOCAL_PREVIEW_PORT);
  });

  it('resolveGatewayCspPort pairs into the disjoint 11000-11999 band', () => {
    expect(resolveGatewayCspPort({ GITHUB_RUN_ID: '345' })).toBe(11_345);
    // Pairing is derived from the REMAPPED preview slot, so the pair moves
    // together: runId 80 → preview 10081 → gateway 11081.
    expect(resolveGatewayCspPort({ GITHUB_RUN_ID: '80' })).toBe(11_081);
  });

  it('resolveGatewayCspPort keeps the local 4173/4174 pairing off-CI', () => {
    expect(resolveGatewayCspPort({})).toBe(LOCAL_GATEWAY_CSP_PORT);
    expect(resolveGatewayCspPort({ GITHUB_RUN_ID: 'garbage' })).toBe(LOCAL_GATEWAY_CSP_PORT);
  });

  it('preview and gateway bands never overlap', () => {
    for (const runId of ['0', '80', '499', '999']) {
      const preview = resolvePreviewPort({ GITHUB_RUN_ID: runId });
      const gateway = resolveGatewayCspPort({ GITHUB_RUN_ID: runId });
      expect(preview).toBeGreaterThanOrEqual(PREVIEW_PORT_BASE);
      expect(preview).toBeLessThan(GATEWAY_CSP_PORT_BASE);
      expect(gateway).toBeGreaterThanOrEqual(GATEWAY_CSP_PORT_BASE);
      expect(gateway).toBeLessThan(GATEWAY_CSP_PORT_BASE + 1_000);
    }
  });
});

describe('defineSmokeConfig `preview` webServer', () => {
  const originalCI = process.env.CI;
  const originalRunId = process.env.GITHUB_RUN_ID;

  beforeEach(() => {
    delete process.env.CI;
    delete process.env.GITHUB_RUN_ID;
  });

  afterEach(() => {
    if (originalCI === undefined) {
      delete process.env.CI;
    } else {
      process.env.CI = originalCI;
    }
    if (originalRunId === undefined) {
      delete process.env.GITHUB_RUN_ID;
    } else {
      process.env.GITHUB_RUN_ID = originalRunId;
    }
  });

  type WebServerBlock = {
    command: string;
    url: string;
    reuseExistingServer?: boolean;
    timeout?: number;
    stdout?: string;
    stderr?: string;
    env?: Record<string, string>;
  };

  const webServerOf = (config: { webServer?: unknown }): WebServerBlock => {
    expect(config.webServer).toBeDefined();
    expect(Array.isArray(config.webServer)).toBe(false);
    return config.webServer as WebServerBlock;
  };

  it('threads ONE resolved port into baseURL + command + url', () => {
    process.env.GITHUB_RUN_ID = '12345';
    const config = defineSmokeConfig({ preview: {} });
    const webServer = webServerOf(config);
    const url = 'http://127.0.0.1:10345';
    expect(config.use?.baseURL).toBe(url);
    expect(webServer.url).toBe(url);
    expect(webServer.command).toContain('--port 10345');
    // The port appears NOWHERE else — no second, desyncable port literal.
    expect(webServer.command.match(/--port \d+/g)).toEqual(['--port 10345']);
  });

  it('always passes --strictPort (the fail-loud half of the fix)', () => {
    const config = defineSmokeConfig({ preview: {} });
    expect(webServerOf(config).command).toContain('--strictPort');
  });

  it('binds 127.0.0.1 (IPv4) to avoid the localhost/::1 probe hang', () => {
    const config = defineSmokeConfig({ preview: {} });
    expect(webServerOf(config).command).toContain('--host 127.0.0.1');
  });

  it('defaults to the local 4173 outside CI', () => {
    const config = defineSmokeConfig({ preview: {} });
    const webServer = webServerOf(config);
    expect(config.use?.baseURL).toBe('http://127.0.0.1:4173');
    expect(webServer.url).toBe('http://127.0.0.1:4173');
    expect(webServer.command).toContain('--port 4173');
  });

  it('composes buildCommand and previewArgs into the command', () => {
    const config = defineSmokeConfig({
      preview: {
        buildCommand: 'npm run build -- --outDir dist --base /',
        previewArgs: ['--outDir', 'dist', '--base', '/'],
      },
    });
    expect(webServerOf(config).command).toBe(
      'npm run build -- --outDir dist --base / && ' +
        'npm run preview -- --outDir dist --base / ' +
        '--host 127.0.0.1 --port 4173 --strictPort'
    );
  });

  it('defaults the build command to npm run build with no extra args', () => {
    const config = defineSmokeConfig({ preview: {} });
    expect(webServerOf(config).command).toBe(
      'npm run build && npm run preview -- --host 127.0.0.1 --port 4173 --strictPort'
    );
  });

  it('sets reuseExistingServer true locally and false in CI', () => {
    const local = defineSmokeConfig({ preview: {} });
    expect(webServerOf(local).reuseExistingServer).toBe(true);

    process.env.CI = 'true';
    const ci = defineSmokeConfig({ preview: {} });
    expect(webServerOf(ci).reuseExistingServer).toBe(false);
  });

  it('pipes webServer stdout/stderr for actionable bind failures', () => {
    const config = defineSmokeConfig({ preview: {} });
    const webServer = webServerOf(config);
    expect(webServer.stdout).toBe('pipe');
    expect(webServer.stderr).toBe('pipe');
  });

  it('defaults the webServer timeout to 180s and honours overrides', () => {
    expect(webServerOf(defineSmokeConfig({ preview: {} })).timeout).toBe(180_000);
    expect(webServerOf(defineSmokeConfig({ preview: { timeout: 120_000 } })).timeout).toBe(120_000);
  });

  it('passes preview env through and omits env when not given', () => {
    const withEnv = defineSmokeConfig({
      preview: { env: { VITE_API_URL: '' } },
    });
    expect(webServerOf(withEnv).env).toEqual({ VITE_API_URL: '' });

    const withoutEnv = defineSmokeConfig({ preview: {} });
    expect('env' in webServerOf(withoutEnv)).toBe(false);
  });

  it('honours an explicit preview.port override', () => {
    process.env.GITHUB_RUN_ID = '12345';
    const config = defineSmokeConfig({ preview: { port: 9_999 } });
    const webServer = webServerOf(config);
    expect(config.use?.baseURL).toBe('http://127.0.0.1:9999');
    expect(webServer.url).toBe('http://127.0.0.1:9999');
    expect(webServer.command).toContain('--port 9999');
  });

  it('lets an explicit baseURL override the derived preview URL', () => {
    const config = defineSmokeConfig({
      baseURL: 'http://127.0.0.1:8080/app',
      preview: {},
    });
    expect(config.use?.baseURL).toBe('http://127.0.0.1:8080/app');
    // The webServer still points at the resolved preview port.
    expect(webServerOf(config).url).toBe('http://127.0.0.1:4173');
  });

  it('rejects preview + webServer together (untyped callers)', () => {
    expect(() =>
      defineSmokeConfig({
        preview: {},
        webServer: { command: 'x', url: 'http://127.0.0.1:1' },
      } as never)
    ).toThrow(/either `preview` or `webServer`/);
  });

  it('rejects a config with neither baseURL nor preview (untyped callers)', () => {
    expect(() => defineSmokeConfig({} as never)).toThrow(/`baseURL` is required/);
  });
});
