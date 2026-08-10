// Injected at document-start only by the Cargo `ui-test` feature. It emulates the Rust command and
// event boundary beneath the production Tauri adapter while keeping the real webview and UI code.

interface TestSession {
  id: string;
  host: string;
  session: string;
  transport: 'local' | 'ssh';
  mode: 'control' | 'plain' | 'local';
  title?: string;
  [key: string]: unknown;
}

interface CreateResult {
  id?: string;
  session?: string;
  mode?: string;
  tmuxPath?: string | null;
  tmuxVersion?: number[] | null;
}

interface TestBackend {
  reject?: Record<string, string>;
  createSessionResult?: CreateResult;
  tunnels?: Record<string, unknown[]>;
  hosts?: string[];
  echoInput?: boolean;
}

interface TestFixture {
  token: string;
  sessions: TestSession[];
  config: { loopbackHosts: string[]; lastActive: string | null };
  backend?: TestBackend;
}

interface CommandArgs {
  meta?: CreateResult;
  id?: string;
  data?: string;
  win?: string;
  cols?: number;
  rows?: number;
  title?: string;
  ids?: string[];
  tabOrder?: string[] | null;
  tabColor?: unknown;
}

interface TestCalls {
  inputs: unknown[][];
  terminal: unknown[][];
  renames: unknown[][];
  tabRenames: unknown[][];
  setLastActive: string[];
  tabSelects: string[];
  reorders: string[][];
  tabPrefs: unknown[][];
  invocations: Array<[string, CommandArgs]>;
}

interface TestEvent<T = unknown> { event: string; payload: T }
type TestEventCallback = (event: TestEvent) => void;

interface TestBridge {
  invoke(command: string, args?: CommandArgs): Promise<unknown>;
  listen(event: string, callback: TestEventCallback): Promise<() => void>;
  emit(event: string, payload: unknown): void;
  calls: TestCalls;
  fixture: TestFixture;
  fixtureKey: string;
  setFixture(fixture: TestFixture): void;
}

interface Window {
  __TAURI__?: { core: { invoke<T>(command: string, args?: CommandArgs): Promise<T> } };
  __wdio_original_core__: { invoke(command: string, args?: CommandArgs): Promise<unknown> };
  __BUOY_UI_TEST__: TestBridge;
  __BUOY_UI_TEST_FIXTURE_TOKEN__: string;
  __fire(event: string, payload: unknown): void;
  __errs: string[];
  __inputs: unknown[][];
  __terminalCalls: unknown[][];
  __renames: unknown[][];
  __tabRenames: unknown[][];
  __setLastActive: string[];
  __tabSelects: string[];
  __reorders: string[][];
  __tabPrefs: unknown[][];
  __invocations: Array<[string, CommandArgs]>;
  __testReset(): Promise<void>;
}

(() => {
  window.requestAnimationFrame = (callback) => window.setTimeout(() => callback(performance.now()), 0);
  window.cancelAnimationFrame = (id) => window.clearTimeout(id);

  // The embedded service asks which window is active before native input. Buoy has one window.
  window.__wdio_original_core__ = {
    invoke(command: string, args?: CommandArgs): Promise<unknown> {
      if (command === 'plugin:wdio|get_window_states') {
        return Promise.resolve([{ label: 'main', title: 'Buoy', is_visible: true, is_focused: true }]);
      }
      if (command === 'plugin:wdio|list_windows') return Promise.resolve(['main']);
      const core = window.__TAURI__?.core;
      return core ? core.invoke(command, args) : Promise.reject(new Error('Tauri core unavailable'));
    },
  };

  const fixtureKey = '__buoy_tauri_ui_fixture__';
  let fixture: TestFixture = {
    token: '', sessions: [], config: { loopbackHosts: ['localhost'], lastActive: null },
  };
  try {
    const prefix = `${fixtureKey}=`;
    if (window.name.startsWith(prefix)) {
      const saved = decodeURIComponent(window.name.slice(prefix.length));
      fixture = { ...fixture, ...(JSON.parse(saved) as Partial<TestFixture>) };
    }
  } catch (_) {}

  const listeners: Record<string, TestEventCallback[]> = Object.create(null) as Record<string, TestEventCallback[]>;
  const clone = <T>(value: T): T => value == null ? value : JSON.parse(JSON.stringify(value)) as T;
  const calls: TestCalls = {
    inputs: [], terminal: [], renames: [], tabRenames: [], setLastActive: [],
    tabSelects: [], reorders: [], tabPrefs: [], invocations: [],
  };
  let createdSessionSequence = 0;

  // Preserve the inspection names used by the WebDriver suites.
  window.__errs = [];
  window.__inputs = calls.inputs;
  window.__terminalCalls = calls.terminal;
  window.__renames = calls.renames;
  window.__tabRenames = calls.tabRenames;
  window.__setLastActive = calls.setLastActive;
  window.__tabSelects = calls.tabSelects;
  window.__reorders = calls.reorders;
  window.__tabPrefs = calls.tabPrefs;
  window.__invocations = calls.invocations;
  window.addEventListener('error', (event) => window.__errs.push(String(event.message)));
  window.addEventListener('unhandledrejection', (event) => {
    const reason = event.reason instanceof Error ? event.reason.message : event.reason;
    window.__errs.push('unhandledrejection: ' + String(reason));
  });

  async function invoke(command: string, args: CommandArgs = {}): Promise<unknown> {
    calls.invocations.push([command, clone(args)]);
    const backend = fixture.backend ?? {};
    const rejection = backend.reject?.[command];
    if (rejection) throw new Error(rejection);
    switch (command) {
      case 'list_sessions': return clone(fixture.sessions);
      case 'get_config': return clone(fixture.config);
      case 'create_session': {
        const meta = args.meta ?? {};
        const configured = clone(backend.createSessionResult ?? {});
        const id = configured.id ?? meta.id ?? `created-${++createdSessionSequence}`;
        return {
          id,
          session: configured.session ?? meta.session ?? `dt-${id}`,
          mode: configured.mode ?? meta.mode ?? 'control',
          tmuxPath: Object.prototype.hasOwnProperty.call(configured, 'tmuxPath')
            ? configured.tmuxPath : '/usr/bin/tmux',
          tmuxVersion: Object.prototype.hasOwnProperty.call(configured, 'tmuxVersion')
            ? configured.tmuxVersion : [3, 6],
        };
      }
      case 'session_input':
        calls.inputs.push([args.id, args.data, args.win]);
        if (backend.echoInput && args.id && args.data) {
          window.setTimeout(() => emit('session:data', { id: args.id, data: args.data, window: args.win }), 0);
        }
        return null;
      case 'session_resize': calls.terminal.push(['resize', args.id, args.cols, args.rows]); return null;
      case 'tab_capture': calls.terminal.push(['capture', args.id, args.win]); return null;
      case 'session_rename': calls.renames.push([args.id, args.title]); return { ok: true, title: args.title };
      case 'tab_rename': calls.tabRenames.push([args.id, args.win, args.title]); return null;
      case 'set_last_active': if (args.id) calls.setLastActive.push(args.id); return null;
      case 'tab_select': if (args.win) calls.tabSelects.push(args.win); return null;
      case 'reorder_sessions': calls.reorders.push((args.ids ?? []).slice()); return null;
      case 'set_tab_prefs':
        calls.tabPrefs.push([args.id, args.tabOrder ? args.tabOrder.slice() : null, clone(args.tabColor)]);
        return null;
      case 'list_tunnels': return clone(backend.tunnels?.[args.id ?? ''] ?? []);
      case 'list_hosts': return clone(backend.hosts ?? []);
      case 'read_remote_file':
      case 'save_file':
      case 'enable_html_scripts':
      case 'open_forwarded_url':
      case 'force_forward': return {};
      default: return null;
    }
  }

  async function listen(event: string, callback: TestEventCallback): Promise<() => void> {
    const eventListeners = listeners[event] ?? (listeners[event] = []);
    eventListeners.push(callback);
    return () => {
      const list = listeners[event] ?? [];
      const index = list.indexOf(callback);
      if (index >= 0) list.splice(index, 1);
    };
  }

  function emit(event: string, payload: unknown): void {
    for (const callback of (listeners[event] ?? []).slice()) callback({ event, payload });
  }

  function setFixture(nextFixture: TestFixture): void {
    fixture = clone(nextFixture);
    createdSessionSequence = 0;
    for (const values of Object.values(calls)) values.length = 0;
    window.__BUOY_UI_TEST__.fixture = fixture;
    window.__BUOY_UI_TEST_FIXTURE_TOKEN__ = fixture.token;
  }

  const eventName = (name: string): string => name.includes(':') ? name : `session:${name}`;
  window.__fire = (event, payload) => emit(eventName(event), payload);
  window.__BUOY_UI_TEST__ = { invoke, listen, emit, calls, fixture, fixtureKey, setFixture };
  window.__BUOY_UI_TEST_FIXTURE_TOKEN__ = fixture.token;
})();
