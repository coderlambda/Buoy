// The transport/session contract is shared by desktop and mobile application packages. Keep UI
// implementation types below local to the renderer; platform runtimes only depend on this package.
export type {
  AppConfig,
  BuoyPlatform,
  CreateSessionMeta,
  CreateSessionResult,
  RemoteFileResult,
  RecoveryTabSnapshot,
  RuntimeCapabilities,
  SessionKind,
  SessionCheckResult,
  SessionMeta,
  SessionMode,
  SessionState,
  SessionTransport,
  TerminalAPI,
  TerminalDataEvent,
  TerminalExitEvent,
  TerminalReadyEvent,
  TerminalStateEvent,
  TunnelEvent,
  TunnelInfo,
  WindowEvent,
} from '@buoy/contracts';

export interface TerminalSize { cols: number; rows: number }

export interface TabContent {
  readonly kind: string;
  readonly mounted: boolean;
  readonly term?: XtermTerminal;
  mount(container: HTMLElement): void | Promise<void>;
  element?(): HTMLElement | null;
  onData(data: string): void;
  fit(): TerminalSize | null;
  resize(cols: number, rows: number): void;
  focus(): void;
  /** Re-render every row of the current buffer. Absent for tab kinds with no cell grid. */
  repaintAllRows?(): void;
  /** Active terminal renderer backend, exposed for diagnostics and measurement. */
  rendererKind?(): string;
  readBuffer(): string;
  dispose(): void;
}

export interface OscNotificationParser {
  write(data: unknown): number;
  reset(): void;
}

export interface TuiActivityTracker {
  /** Feed a raw PTY chunk and report whether the pane is TUI-active at `now`. */
  write(data: string, now?: number): boolean;
  /** Query activity without feeding output. */
  active(now?: number): boolean;
  reset(): void;
}

export interface TuiActivityOptions {
  decayMs?: number;
  now?: () => number;
}
