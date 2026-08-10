export type SessionKind = 'local' | 'remote';
export type SessionTransport = 'local' | 'ssh';
export type SessionMode = 'control' | 'plain' | 'local';
export type SessionState = 'idle' | 'connecting' | 'connected' | 'reconnecting' | 'dead' | 'closed';

export interface SessionMeta {
  id: string;
  host: string;
  session: string;
  transport: SessionTransport;
  mode: SessionMode;
  kind?: SessionKind;
  title?: string;
  tmuxPath?: string;
  tmuxVersion?: number[];
  order?: number;
  color?: string | null;
  lastTab?: string | null;
  tabOrder?: string[];
  tabColors?: Record<string, string>;
}

export interface CreateSessionMeta {
  id?: string;
  session?: string;
  host: string;
  kind: SessionKind;
  transport: SessionTransport;
  mode: SessionMode;
  title: string;
  tmuxPath?: string;
  tmuxVersion?: number[];
}

export interface CreateSessionResult {
  id: string;
  session: string;
  mode?: SessionMode;
  tmuxPath?: string;
  tmuxVersion?: number[];
}

export interface TunnelInfo {
  remote: number;
  local: number;
  active?: boolean;
}

export interface AppConfig {
  loopbackHosts?: string[];
  lastActive?: string | null;
}

export interface RemoteFileResult {
  data_b64: string;
  size: number;
  truncated?: boolean;
}

export interface TerminalDataEvent { id: string; data: string; window?: string | null }
export interface TerminalStateEvent { id: string; state: SessionState }
export interface TerminalExitEvent { id: string }
export interface TerminalReadyEvent { id: string }
export interface TunnelEvent { id: string; tunnels: TunnelInfo[] }
export interface WindowEvent {
  id: string;
  action: 'add' | 'close' | 'rename' | 'active';
  window: string;
  name?: string;
  order?: string[];
}

export interface TerminalAPI {
  listSessions(): Promise<SessionMeta[]>;
  createSession(meta: CreateSessionMeta): Promise<CreateSessionResult>;
  input(id: string, data: string, win?: string | null): Promise<unknown> | void;
  resize(id: string, cols: number, rows: number): Promise<unknown> | void;
  ack(id: string, bytes: number): void;
  close(id: string): Promise<unknown> | void;
  kill(id: string): Promise<{ killedRemote?: boolean }> | void;
  retry(id: string): Promise<unknown> | void;
  forceReconnect(id: string): Promise<unknown> | void;
  rename(id: string, title: string): Promise<{ ok: boolean; title: string }>;
  openExternal(url: string): Promise<unknown> | void;
  copyText(text: string): Promise<void>;
  readRemoteFile(id: string, path: string): Promise<RemoteFileResult>;
  saveFile(dataB64: string, suggestedName: string): Promise<{ ok?: boolean }>;
  enableHtmlScripts(dataB64: string): Promise<{ url?: string }>;
  openForwardedUrl(id: string, url: string): Promise<{ ok?: boolean; localUrl?: string }>;
  getConfig(): Promise<AppConfig>;
  listTunnels(id: string): Promise<TunnelInfo[]>;
  closeTunnel(id: string, remote: number): Promise<unknown> | void;
  forceForward(id: string, remote: number): Promise<unknown>;
  listHosts(): Promise<string[]>;
  rememberHost(host: string): Promise<unknown> | void;
  tabNew(id: string): Promise<unknown> | void;
  tabSelect(id: string, win: string): Promise<unknown> | void;
  tabClose(id: string, win: string): Promise<unknown> | void;
  tabCapture(id: string, win: string): Promise<unknown> | void;
  tabRename(id: string, win: string, title: string): Promise<unknown> | void;
  reorderSessions(ids: string[]): Promise<unknown>;
  setSessionColor(id: string, color: string | null): Promise<unknown>;
  setLastActive(id: string): Promise<unknown>;
  setLastTab(id: string, win: string): Promise<unknown>;
  setTabPrefs(
    id: string,
    tabOrder: string[] | null,
    tabColor: [string, string | null] | null,
  ): Promise<unknown>;
  onData(callback: (event: TerminalDataEvent) => void): void;
  onState(callback: (event: TerminalStateEvent) => void): void;
  onIntentionalExit(callback: (event: TerminalExitEvent) => void): void;
  onWindow(callback: (event: WindowEvent) => void): void;
  onReady(callback: (event: TerminalReadyEvent) => void): void;
  onTunnels(callback: (event: TunnelEvent) => void): void;
  log(message: string): void;
}

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
