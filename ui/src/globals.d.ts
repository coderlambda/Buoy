interface XtermBufferLine {
  translateToString(trimRight?: boolean): string;
  getCell(index: number): {
    getChars(): string;
    isUnderline(): boolean | number;
  } | undefined;
}

interface XtermBuffer {
  readonly length: number;
  readonly baseY: number;
  readonly cursorX: number;
  readonly cursorY: number;
  getLine(index: number): XtermBufferLine | undefined;
}

interface XtermLink {
  range: {
    start: { x: number; y: number };
    end: { x: number; y: number };
  };
  text: string;
  decorations?: { underline?: boolean; pointerCursor?: boolean };
  activate(event: MouseEvent, text: string): void;
  hover?(event: MouseEvent, text: string): void;
  leave?(event: MouseEvent, text: string): void;
}

interface XtermLinkProvider {
  provideLinks(lineNumber: number, callback: (links: XtermLink[] | undefined) => void): void;
}

interface XtermTerminal {
  readonly cols: number;
  readonly rows: number;
  readonly buffer: { active: XtermBuffer };
  readonly parser?: { registerOscHandler(code: number, callback: (payload: string) => boolean): unknown };
  loadAddon(addon: unknown): void;
  registerLinkProvider(provider: XtermLinkProvider): void;
  onData(callback: (data: string) => void): unknown;
  onBell?(callback: () => void): unknown;
  attachCustomKeyEventHandler(callback: (event: KeyboardEvent) => boolean): void;
  getSelection(): string;
  hasSelection(): boolean;
  open(element: HTMLElement): void;
  input(data: string, wasUserInput?: boolean): void;
  write(data: string, callback?: () => void): void;
  resize(cols: number, rows: number): void;
  /** Re-render rows start..end of the current buffer. */
  refresh(start: number, end: number): void;
  focus(): void;
  blur(): void;
  dispose(): void;
}

interface XtermTerminalOptions {
  fontFamily?: string;
  fontSize?: number;
  lineHeight?: number;
  rescaleOverlappingGlyphs?: boolean;
  theme?: { background?: string; foreground?: string };
  scrollback?: number;
  linkHandler?: unknown;
}

declare const Terminal: new (options?: XtermTerminalOptions) => XtermTerminal;
declare namespace FitAddon {
  class FitAddon {
    fit(): void;
  }
}
declare namespace CanvasAddon {
  class CanvasAddon {
    dispose(): void;
    clearTextureAtlas(): void;
  }
}

interface TauriEvent<T> { payload: T }
interface TauriCore {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
}
interface TauriEvents {
  listen<T>(event: string, callback: (event: TauriEvent<T>) => void): Promise<() => void>;
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

interface Window {
  __TAURI__: { core: TauriCore; event: TauriEvents };
  __BUOY_UI_TEST__?: {
    invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
    listen<T>(event: string, callback: (event: TauriEvent<T>) => void): Promise<() => void>;
  };
  terminalAPI: import('./types.js').TerminalAPI;
  dtPlugins: {
    registerLink: import('./plugins.js').PluginRegistry['registerLink'];
    registerTabKind: import('./plugins.js').PluginRegistry['registerTabKind'];
  };
  __testType(data: string): void;
  __testInputReady(): boolean;
  __testMount(id: string): void;
  __testDispose(id: string): void;
  __testReadBuffer(): string;
  __testTextIsUnderlined(text: string): boolean | null;
  __testLinkPath(text: string): string | null;
  __testRepaintCount(): number;
  __testRendererKind(): string | null;
  __testBenchmarkWrite(lines: number): Promise<RendererWriteBenchmark>;
  __testBenchmarkFrames(frames: number): Promise<RendererFrameBenchmark>;
  __testArmInputLatency(): void;
  __testSendInput(data: string): void;
  __testInputLatency(): number | null;
  __testTerminalState(): {
    cols: number;
    rows: number;
    cursorX: number;
    cursorY: number;
    baseY: number;
    line: string;
    previous: string;
    next: string;
  } | null;
  __testReset(): Promise<void>;
}
