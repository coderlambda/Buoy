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
  __testRepaintCount(): number;
  __testRendererKind(): string | null;
  __testFindText(text: string): {
    x: number;
    y: number;
    isWrapped: boolean;
    underlined: boolean;
  } | null;
  __testTabTerminalSizes(): Array<{
    winId: string;
    cols: number;
    rows: number;
    active: boolean;
  }>;
  __testBenchmarkWrite(lines: number): Promise<RendererWriteBenchmark>;
  __testBenchmarkFrames(frames: number): Promise<RendererFrameBenchmark>;
  __testArmInputLatency(): void;
  __testSendInput(data: string): void;
  __testInputLatency(): number | null;
  __BUOY_UI_TEST__: { setFixture(fixture: Record<string, unknown>): void };
}

interface RendererWriteBenchmark {
  bytes: number;
  lines: number;
  parseMs: number;
  totalMs: number;
}

interface RendererFrameBenchmark {
  frames: number;
  meanMs: number;
  p95Ms: number;
  maxMs: number;
}

declare namespace WebdriverIO {
  interface Capabilities {
    'tauri:options'?: { application: string };
  }
}
