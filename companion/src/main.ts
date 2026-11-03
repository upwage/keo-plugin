// The panel. Plain TypeScript over Tauri's global API (app.withGlobalTauri); every action is one
// of the app's own commands, and the app pushes a `status` event on every change.

import { clearErrorIn, showErrorIn } from './feedback.js';
import { headerState, sessionRows } from './sessions.js';

interface ToolView {
  tool: string;
  name: string;
  installed: boolean;
  version: string | null;
  notice: string;
}

interface Session {
  tool: string;
  session_id: string;
  cwd: string;
  name: string;
  state: string;
  active_at?: number | null;
}

interface Status {
  state: string;
  tray: string;
  signed_in: boolean;
  signing_in: boolean;
  paused: boolean;
  start_at_login: boolean;
  first_run_done: boolean;
  approved_dirs: string[];
  tools: ToolView[];
  sessions: Session[];
  host: string;
  version: string;
  error: string | null;
}

interface TauriGlobal {
  core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
  event: { listen<T>(name: string, cb: (e: { payload: T }) => void): Promise<() => void> };
}

const tauri = (window as unknown as { __TAURI__: TauriGlobal }).__TAURI__;
const invoke = tauri.core.invoke;

const STATE_TEXT: Record<string, string> = {
  first_run: 'Welcome. Sign in with Keo to connect this machine.',
  signed_out: 'Signed out.',
  registering: 'Connecting to Keo...',
  listening: 'Listening for tasks from Keo.',
  paused: 'Paused. Keo will not start or message sessions.',
  pulling: 'Fetching a task...',
  acting: 'Working on a task.',
  relaying: 'A session needs your attention.',
  needs_update: 'A coding tool or Keo Companion needs an update.',
};

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

function item(text: string, extra?: HTMLElement): HTMLLIElement {
  const li = document.createElement('li');
  const span = document.createElement('span');
  span.textContent = text;
  li.append(span);
  if (extra) li.append(extra);
  return li;
}

let lastSessions: Session[] = [];

/** The session rows alone: the minute timer redraws only these, so an error line another
 *  command showed stays put. */
function renderSessions(list: Session[]): void {
  lastSessions = list;
  const { rows, more } = sessionRows(list, Date.now() / 1000);
  const sessions = $('sessions');
  sessions.replaceChildren(
    ...rows.map((r) => {
      const li = document.createElement('li');
      li.className = 'session';
      const text = document.createElement('div');
      const title = document.createElement('div');
      title.textContent = r.title;
      const detail = document.createElement('div');
      detail.className = 'muted detail';
      detail.textContent = r.detail;
      text.append(title, detail);
      const st = document.createElement('span');
      st.className = r.attention ? 'attention' : 'muted';
      st.textContent = r.state;
      li.append(text, st);
      return li;
    }),
  );
  $('more-sessions').hidden = more === 0;
  $('more-sessions').textContent = `+${more} more`;
  $('no-sessions').hidden = list.length > 0;
}

function render(s: Status): void {
  $('status').textContent = s.signing_in
    ? 'Finish signing in in your browser.'
    : (STATE_TEXT[headerState(s.state, s.sessions)] ?? s.state);
  const err = $('error');
  err.hidden = !s.error;
  err.textContent = s.error ?? '';

  // While the browser sign-in waits, Sign in stays clickable (it starts over) and Cancel ends it.
  const signIn = $<HTMLButtonElement>('sign-in');
  signIn.hidden = s.signed_in;
  signIn.textContent = s.signing_in ? 'Start over' : 'Sign in with Keo';
  $<HTMLButtonElement>('cancel-sign-in').hidden = !s.signing_in;
  $<HTMLButtonElement>('sign-out').hidden = !s.signed_in;

  const tools = $('tools');
  tools.replaceChildren(
    ...s.tools.map((t) => {
      const note = document.createElement('span');
      note.textContent = t.notice || (t.version ?? '');
      if (t.notice && t.installed) note.className = 'notice';
      else note.className = 'muted';
      return item(t.name, note);
    }),
  );

  renderSessions(s.sessions);

  $<HTMLInputElement>('paused').checked = s.paused;
  $<HTMLInputElement>('paused').disabled = !s.signed_in;
  $<HTMLInputElement>('start-at-login').checked = s.start_at_login;

  const dirs = $('dirs');
  dirs.replaceChildren(
    ...s.approved_dirs.map((d) => {
      const rm = document.createElement('button');
      rm.type = 'button';
      rm.className = 'secondary';
      rm.textContent = 'Remove';
      rm.addEventListener('click', () => void act('remove_dir', { path: d }));
      return item(d, rm);
    }),
  );

  $('version').textContent =
    `Version ${s.version}. Connected host: ${s.host.replace('https://', '')}`;
}

function showError(e: unknown, slot = 'error'): void {
  showErrorIn($(slot), e);
}

async function refresh(): Promise<void> {
  render(await invoke<Status>('get_status'));
}

/** Run a command; a refusal shows in the `slot` error line (the header one by default). */
async function act(
  cmd: string,
  args?: Record<string, unknown>,
  slot = 'error',
): Promise<boolean> {
  try {
    const s = await invoke<Status | null>(cmd, args);
    if (s) render(s);
    else await refresh();
    return true;
  } catch (e) {
    // A refused command changed nothing: redraw from the saved status, so a switch the person
    // just flipped (Pause, Start at login) shows what is really in effect, then show why.
    try {
      await refresh();
    } catch {
      // The error below still shows.
    }
    showError(e, slot);
    return false;
  }
}

async function loadLog(): Promise<void> {
  try {
    const lines = await invoke<string[]>('log_tail', { lines: 100 });
    const pre = $('log');
    pre.textContent = lines.join('\n');
    pre.scrollTop = pre.scrollHeight;
  } catch (e) {
    showError(e);
  }
}

function wire(): void {
  $('sign-in').addEventListener('click', () => void act('sign_in'));
  $('sign-out').addEventListener('click', () => void act('sign_out'));
  $('quit').addEventListener('click', () => void act('quit'));
  $('cancel-sign-in').addEventListener('click', () => void act('cancel_sign_in'));
  $<HTMLInputElement>('paused').addEventListener(
    'change',
    (e) => void act('set_paused', { paused: (e.target as HTMLInputElement).checked }),
  );
  $<HTMLInputElement>('start-at-login').addEventListener(
    'change',
    (e) => void act('set_start_at_login', { enabled: (e.target as HTMLInputElement).checked }),
  );
  $<HTMLFormElement>('add-dir').addEventListener('submit', (e) => {
    e.preventDefault();
    const input = $<HTMLInputElement>('dir-input');
    const path = input.value.trim();
    if (!path) return;
    clearErrorIn($('dir-error'));
    void act('add_dir', { path }, 'dir-error').then((ok) => {
      if (ok && input.value.trim() === path) input.value = '';
    });
  });
  $('dir-input').addEventListener('input', () => clearErrorIn($('dir-error')));
  $('log-refresh').addEventListener('click', () => void loadLog());
}

wire();
// Register the listener first, so a push that lands during the first refresh is not missed.
void tauri.event
  .listen<Status>('status', (e) => render(e.payload))
  .catch(() => undefined)
  .then(() => refresh());
void loadLog();
// Ages ("5m ago") move with the clock, not only with a status push.
setInterval(() => renderSessions(lastSessions), 60_000);
