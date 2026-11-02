// Error text for the panel, kept free of the DOM so a test can drive it with plain objects.

/** The part of an element the panel's error lines use. */
export interface ErrorSlot {
  hidden: boolean;
  textContent: string | null;
}

/** What to show for a refused command: Tauri sends a command's error as a plain string. */
export function errorText(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

/** Show why a command was refused. */
export function showErrorIn(slot: ErrorSlot, e: unknown): void {
  slot.hidden = false;
  slot.textContent = errorText(e);
}

/** Hide an error line once its cause is gone. */
export function clearErrorIn(slot: ErrorSlot): void {
  slot.hidden = true;
  slot.textContent = '';
}
