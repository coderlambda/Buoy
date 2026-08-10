interface UiTestInvocation {
  0: string;
  1?: {
    meta?: {
      id?: unknown;
      kind?: unknown;
      transport?: unknown;
      mode?: unknown;
      title?: unknown;
      host?: unknown;
      [key: string]: unknown;
    };
    [key: string]: unknown;
  };
}

interface Window {
  __BUOY_UI_TEST_FIXTURE_TOKEN__: string;
  __fire(event: string, payload: Record<string, unknown>): void;
  __invocations: UiTestInvocation[];
  __inputs: unknown[][];
  __errs: string[];
  __renames: unknown[][];
  __setLastActive: string[];
  __tabSelects: unknown[][];
  __tabRenames: unknown[][];
  __cancelNextTestPointer?: boolean;
  __testReset(): Promise<void>;
  __BUOY_UI_TEST__: { setFixture(fixture: Record<string, unknown>): void };
}

declare namespace WebdriverIO {
  interface Capabilities {
    'tauri:options'?: { application: string };
  }
}
