export type CodexClosePreference = "ask" | "graceful" | "force";

export const CODEX_CLOSE_PREFERENCE_STORAGE_KEY = "codex-close-preference";

export function parseCodexClosePreference(value: string | null): CodexClosePreference {
  // Old force-close preferences must never opt a user into terminating work.
  return value === "ask" ? "ask" : "graceful";
}

export function rememberedCodexClosePreference(
  _forceClose: boolean,
  remember: boolean,
): CodexClosePreference {
  if (!remember) return "ask";
  return "graceful";
}

interface CodexCloseProcessState {
  count: number;
  external_count: number;
}

// Missing classification (for example, from an older backend) fails closed.
export function canCloseCodexDesktop(info: CodexCloseProcessState | null): boolean {
  return info !== null
    && Number.isInteger(info.count)
    && info.count > 0
    && info.external_count === 0;
}
