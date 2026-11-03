// The panel's session list, DOM-free so a spec can run it: one row per session, the ones that
// need a person first, then working, then the newest activity; at most `cap` rows and a
// "+N more" line for the rest.

export interface SessionLike {
  tool: string;
  session_id: string;
  cwd: string;
  name: string;
  state: string;
  active_at?: number | null;
}

export interface Row {
  key: string;
  title: string;
  detail: string;
  state: string;
  attention: boolean;
}

export const SESSION_CAP = 8;

const TOOL_TEXT: Record<string, string> = { codex: 'Codex', 'claude-code': 'Claude Code' };

const STATE_TEXT: Record<string, string> = {
  waiting_permission: 'needs you',
  working: 'working',
  started: 'starting',
  idle: 'idle',
  finished: 'done',
};

const RANK: Record<string, number> = { waiting_permission: 0, working: 1, started: 1 };

export function needsAttention(s: SessionLike): boolean {
  return s.state === 'waiting_permission';
}

/** "just now", "5m ago", "3h ago", "2d ago"; empty when the time is unknown. */
export function age(at: number | null | undefined, now: number): string {
  if (!at) return '';
  const secs = Math.max(0, now - at);
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86_400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86_400)}d ago`;
}

function folder(cwd: string): string {
  const parts = cwd.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? cwd;
}

/** The rows to show, and how many sessions the cap left out. `now` is Unix seconds. */
export function sessionRows(
  sessions: SessionLike[],
  now: number,
  cap = SESSION_CAP,
): { rows: Row[]; more: number } {
  const seen = new Set<string>();
  const unique = sessions.filter((s) => {
    const key = `${s.tool}:${s.session_id}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  unique.sort(
    (a, b) =>
      (RANK[a.state] ?? 2) - (RANK[b.state] ?? 2) || (b.active_at ?? 0) - (a.active_at ?? 0),
  );
  const rows = unique.slice(0, cap).map((s) => ({
    key: `${s.tool}:${s.session_id}`,
    title: s.name || s.session_id,
    detail: [TOOL_TEXT[s.tool] ?? s.tool, folder(s.cwd), age(s.active_at, now)]
      .filter(Boolean)
      .join(' · '),
    state: STATE_TEXT[s.state] ?? s.state,
    attention: needsAttention(s),
  }));
  return { rows, more: unique.length - rows.length };
}

/**
 * The header's state. "Needs your attention" only while a listed session does: a question the
 * app still counts as open with no session waiting reads as working instead.
 */
export function headerState(state: string, sessions: SessionLike[]): string {
  if (state === 'relaying' && !sessions.some(needsAttention)) return 'acting';
  return state;
}
