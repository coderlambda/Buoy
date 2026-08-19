
import * as path from 'node:path';

const binary = path.resolve(
  __dirname,
  'target',
  'debug',
  process.platform === 'win32' ? 'buoy.exe' : 'buoy',
);

export const config: WebdriverIO.Config = {
  runner: 'local',
  specs: [
    './test/gui-rename.ts',
    './test/gui-reorder.ts',
    './test/gui-notifications.ts',
    './test/gui-new-session.ts',
    './test/gui-terminal-repaint.ts',
    './test/gui-session-history.ts',
    './test/gui-mobile-shell.ts',
  ],
  maxInstances: 1,
  capabilities: [{
    browserName: 'tauri',
    'tauri:options': { application: binary },
  }],
  services: [['@wdio/tauri-service', {
    appBinaryPath: binary,
    driverProvider: 'embedded',
    embeddedPort: 4445,
    captureBackendLogs: false,
    captureFrontendLogs: false,
  }]],
  framework: 'mocha',
  reporters: ['spec'],
  // The current service diagnostics still probes external tauri-driver even for the embedded
  // provider. Keep that irrelevant warning out of an otherwise dependency-free test run.
  logLevel: 'silent',
  bail: 0,
  waitforTimeout: 10000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 1,
  mochaOpts: { ui: 'bdd', timeout: 60000 },
};
