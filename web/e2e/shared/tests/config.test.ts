import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { ALL_BROWSER_PROJECTS, DEFAULT_BROWSER_PROJECTS, defineSmokeConfig } from '../config';

describe('defineSmokeConfig', () => {
  const originalCI = process.env.CI;

  beforeEach(() => {
    delete process.env.CI;
  });

  afterEach(() => {
    if (originalCI === undefined) {
      delete process.env.CI;
    } else {
      process.env.CI = originalCI;
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
    // shared use defaults
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
