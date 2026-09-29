import { useState, useEffect, useCallback, useMemo, useRef } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useAccounts } from "./hooks/useAccounts";
import { useDesktopReopen } from "./hooks/useDesktopReopen";
import { useCodexClosePreference } from "./hooks/useCodexClosePreference";
import { SettingsModal } from "./components/SettingsModal";
import { finishForceClose, type DesktopReopenPreference } from "./lib/desktopReopen";
import { canCloseCodexDesktop, type CodexClosePreference } from "./lib/codexClosePreference";
import { useForceCloseCodexProcesses } from "./hooks/useForceCloseCodexProcesses";
import { AccountCard, AddAccountModal, UpdateChecker } from "./components";
import type { AccountWithUsage, CodexProcessInfo, DockDisplayMode, UsageInfo } from "./types";
import {
  exportFullBackupFile,
  importFullBackupFile,
  isTauriRuntime,
  invokeBackend,
} from "./lib/platform";
import {
  applyTheme,
  readStoredTheme,
  THEME_CHANGED_EVENT,
  THEME_STORAGE_KEY,
  type ThemeMode,
} from "./lib/theme";
import {
  AUTO_WARMUP_ACCOUNTS_STORAGE_KEY,
  AUTO_WARMUP_ALL_CHANGED_EVENT,
  AUTO_WARMUP_LEDGER_STORAGE_KEY,
  TIMED_WARMUP_LEDGER_STORAGE_KEY,
  normalizeTimedWarmupTimes,
  readAutoWarmupAllEnabled,
  readTimedWarmupEnabled,
  readTimedWarmupTimes,
  writeAutoWarmupAllEnabled,
  writeTimedWarmupEnabled,
  writeTimedWarmupTimes,
} from "./lib/autoWarmup";
import {
  getAutoWarmupWindowKey,
  getAutoWarmupWindowKind,
  getDueAutoWarmupWindow,
  type AutoWarmupWindow,
  type AutoWarmupWindowKind,
} from "./lib/autoWarmupPolicy";
import { ProcessPanel } from "./components/ProcessPanel";
import { HoverHelp } from "./components/HoverHelp";
import "./App.css";

const AUTO_WARMUP_CHECK_INTERVAL_MS = 30 * 1000;
const AUTO_WARMUP_RETRY_BACKOFF_MS = 60 * 1000;
const LIMIT_FULL_THRESHOLD = 99.5;
const ACCOUNT_SEARCH_THRESHOLD = 8;
const SWITCH_ACCOUNT_BLOCKED_EVENT = "switch-account-blocked";
const CLOSE_BEHAVIOR_REQUESTED_EVENT = "close-behavior-requested";
interface SwitchAccountBlockedPayload {
  accountId?: string;
  error?: string;
}
interface CloseBehaviorRequestedPayload {
  requestId?: number;
}
type AutoWarmupLedger = Record<
  string,
  {
    lastSuccessfulWarmupAt?: number;
    lastAutoWindowKey?: string;
    lastAutoWindowKind?: AutoWarmupWindowKind;
  }
>;
const appWindow = getCurrentWindow();
const isMacOs =
  typeof navigator !== "undefined" &&
  /(Mac|iPhone|iPod|iPad)/i.test(navigator.userAgent);

function readStoredStringArray(key: string): string[] {
  if (typeof window === "undefined") return [];
  try {
    const parsed = JSON.parse(window.localStorage.getItem(key) ?? "[]");
    return Array.isArray(parsed) ? parsed.filter((item) => typeof item === "string") : [];
  } catch {
    return [];
  }
}

function readStoredAutoWarmupLedger(): AutoWarmupLedger {
  if (typeof window === "undefined") return {};
  try {
    const parsed = JSON.parse(window.localStorage.getItem(AUTO_WARMUP_LEDGER_STORAGE_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};

    const entries: Array<[string, AutoWarmupLedger[string]]> = [];
    for (const [accountId, value] of Object.entries(parsed)) {
      if (!value || typeof value !== "object" || Array.isArray(value)) continue;

      const entry: AutoWarmupLedger[string] = {};
      if (
        "lastSuccessfulWarmupAt" in value &&
        typeof value.lastSuccessfulWarmupAt === "number"
      ) {
        entry.lastSuccessfulWarmupAt = value.lastSuccessfulWarmupAt;
      }
      if ("lastAutoWindowKey" in value && typeof value.lastAutoWindowKey === "string") {
        entry.lastAutoWindowKey = value.lastAutoWindowKey;
      }
      if (
        "lastAutoWindowKind" in value &&
        (value.lastAutoWindowKind === "session" || value.lastAutoWindowKind === "weekly")
      ) {
        entry.lastAutoWindowKind = value.lastAutoWindowKind;
      }

      if (Object.keys(entry).length > 0) entries.push([accountId, entry]);
    }
    return Object.fromEntries(entries);
  } catch {
    return {};
  }
}

function readStoredTimedWarmupLedger(): Record<string, string> {
  if (typeof window === "undefined") return {};
  try {
    const parsed = JSON.parse(window.localStorage.getItem(TIMED_WARMUP_LEDGER_STORAGE_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return Object.fromEntries(
      Object.entries(parsed).filter(
        (entry): entry is [string, string] =>
          typeof entry[0] === "string" && typeof entry[1] === "string"
      )
    );
  } catch {
    return {};
  }
}

function isLimitFull(usedPercent: number | null | undefined): boolean {
  return usedPercent !== null && usedPercent !== undefined && usedPercent >= LIMIT_FULL_THRESHOLD;
}

function getPreferredUsedPercent(usage: UsageInfo | undefined): number | null | undefined {
  return usage?.primary_used_percent ?? usage?.secondary_used_percent;
}

function getPreferredResetsAt(usage: UsageInfo | undefined): number | null | undefined {
  return usage?.primary_resets_at ?? usage?.secondary_resets_at;
}

function getTimedWarmupTargets(accounts: AccountWithUsage[]): AccountWithUsage[] {
  return accounts.filter(
    (account) =>
      account.usage &&
      !account.usageLoading &&
      !account.usage.error &&
      !isLimitFull(account.usage.secondary_used_percent)
  );
}

function matchesAccountSearch(
  account: AccountWithUsage,
  normalizedQuery: string
): boolean {
  if (!normalizedQuery) return true;

  return (
    account.name.toLowerCase().includes(normalizedQuery) ||
    account.email?.toLowerCase().includes(normalizedQuery) === true
  );
}

function App() {
  const {
    accounts,
    loading,
    error,
    loadAccounts,
    refreshUsage,
    refreshSingleUsage,
    warmupAccount,
    warmupAllAccounts,
    switchAccount,
    deleteAccount,
    renameAccount,
    importFromFile,
    exportAccountsSlimText,
    importAccountsSlimText,
    startOAuthLogin,
    completeOAuthLogin,
    cancelOAuthLogin,
    loadMaskedAccountIds,
    saveMaskedAccountIds,
  } = useAccounts();

  const [isAddModalOpen, setIsAddModalOpen] = useState(false);
  const [isConfigModalOpen, setIsConfigModalOpen] = useState(false);
  const [configModalMode, setConfigModalMode] = useState<"slim_export" | "slim_import">(
    "slim_export"
  );
  const [configPayload, setConfigPayload] = useState("");
  const [configModalError, setConfigModalError] = useState<string | null>(null);
  const [configCopied, setConfigCopied] = useState(false);
  const [switchingId, setSwitchingId] = useState<string | null>(null);
  const [deleteConfirmId, setDeleteConfirmId] = useState<string | null>(null);
  const [processInfo, setProcessInfo] = useState<CodexProcessInfo | null>(null);
  const [pendingSwitchAccountId, setPendingSwitchAccountId] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [isOpeningCodex, setIsOpeningCodex] = useState(false);
  const [isExportingSlim, setIsExportingSlim] = useState(false);
  const [isImportingSlim, setIsImportingSlim] = useState(false);
  const [isExportingFull, setIsExportingFull] = useState(false);
  const [isImportingFull, setIsImportingFull] = useState(false);
  const [isWarmingAll, setIsWarmingAll] = useState(false);
  const [warmingUpId, setWarmingUpId] = useState<string | null>(null);
  const [refreshSuccess, setRefreshSuccess] = useState(false);
  const [warmupToast, setWarmupToast] = useState<{
    message: string;
    isError: boolean;
  } | null>(null);
  const [autoWarmupAllEnabled, setAutoWarmupAllEnabled] = useState(() => {
    return readAutoWarmupAllEnabled();
  });
  const [autoWarmupAccountIds, setAutoWarmupAccountIds] = useState<Set<string>>(
    () => new Set(readStoredStringArray(AUTO_WARMUP_ACCOUNTS_STORAGE_KEY))
  );
  const [autoWarmupLedger, setAutoWarmupLedger] =
    useState<AutoWarmupLedger>(() => readStoredAutoWarmupLedger());
  const [autoWarmupRunningIds, setAutoWarmupRunningIds] = useState<Set<string>>(
    new Set()
  );
  const [timedWarmupEnabled, setTimedWarmupEnabled] = useState(() =>
    readTimedWarmupEnabled()
  );
  const [timedWarmupTimes, setTimedWarmupTimes] = useState<string[]>(() =>
    readTimedWarmupTimes()
  );
  const [isTimedWarmupOpen, setIsTimedWarmupOpen] = useState(false);
  const [timedWarmupRunning, setTimedWarmupRunning] = useState(false);
  const [timedWarmupDraft, setTimedWarmupDraft] = useState("");
  const [maskedAccounts, setMaskedAccounts] = useState<Set<string>>(new Set());
  const [accountSearchQuery, setAccountSearchQuery] = useState("");
  const [isAccountSearchOpen, setIsAccountSearchOpen] = useState(false);
  const isAccountSearchEnabled = accounts.length >= ACCOUNT_SEARCH_THRESHOLD;
  const [otherAccountsSort, setOtherAccountsSort] = useState<
    | "deadline_asc"
    | "deadline_desc"
    | "remaining_desc"
    | "remaining_asc"
    | "subscription_asc"
    | "subscription_desc"
  >("deadline_asc");
  const [isActionsMenuOpen, setIsActionsMenuOpen] = useState(false);
  const [isNavMenuOpen, setIsNavMenuOpen] = useState(false);
  const [isSettingsOpen, setIsSettingsOpen] = useState(false);
  const [isProcessPanelOpen, setIsProcessPanelOpen] = useState(false);
  const [isCompletingForceClose, setIsCompletingForceClose] = useState(false);
  const toastTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const forceCloseInFlightRef = useRef(false);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const stop = await listen("desktop-reopen-settings-requested", () => {
        setIsSettingsOpen(true);
      });
      if (disposed) stop();
      else unlisten = stop;
    }).catch((err) => console.error("Failed to listen for settings requests:", err));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const actionsMenuRef = useRef<HTMLDivElement | null>(null);
  const navMenuRef = useRef<HTMLDivElement | null>(null);
  const [themeMode, setThemeMode] = useState<ThemeMode>(readStoredTheme);
  const [isWindowMaximized, setIsWindowMaximized] = useState(false);
  const [closeBehaviorPromptOpen, setCloseBehaviorPromptOpen] = useState(false);
  const [closeBehaviorDontAskAgain, setCloseBehaviorDontAskAgain] = useState(false);
  const [isCompletingCloseBehavior, setIsCompletingCloseBehavior] = useState(false);
  const accountsRef = useRef(accounts);
  const autoWarmupAccountIdsRef = useRef(autoWarmupAccountIds);
  const autoWarmupLedgerRef = useRef(autoWarmupLedger);
  const autoWarmupRunningIdsRef = useRef(autoWarmupRunningIds);
  const autoWarmupRetryAfterRef = useRef<Record<string, number>>({});
  const timedWarmupRunningRef = useRef(timedWarmupRunning);
  // Tracks the last calendar date (YYYY-MM-DD) each scheduled time fired on,
  // so each time triggers at most once per day.
  const timedWarmupLastFireRef = useRef<Record<string, string>>(readStoredTimedWarmupLedger());

  useEffect(() => {
    accountsRef.current = accounts;
  }, [accounts]);

  useEffect(() => {
    if (!isAccountSearchEnabled && accountSearchQuery) {
      setAccountSearchQuery("");
    }
  }, [accountSearchQuery, isAccountSearchEnabled]);

  useEffect(() => {
    autoWarmupAccountIdsRef.current = autoWarmupAccountIds;
  }, [autoWarmupAccountIds]);

  useEffect(() => {
    autoWarmupRunningIdsRef.current = autoWarmupRunningIds;
  }, [autoWarmupRunningIds]);

  useEffect(() => {
    timedWarmupRunningRef.current = timedWarmupRunning;
  }, [timedWarmupRunning]);

  useEffect(() => {
    try {
      writeTimedWarmupEnabled(timedWarmupEnabled);
    } catch {
      // Ignore storage errors; timed warm-up still works for the current session.
    }
  }, [timedWarmupEnabled]);

  useEffect(() => {
    try {
      writeTimedWarmupTimes(timedWarmupTimes);
    } catch {
      // Ignore storage errors; timed warm-up still works for the current session.
    }
  }, [timedWarmupTimes]);

  useEffect(() => {
    if (loading || error) return;

    const validAccountIds = new Set(accounts.map((account) => account.id));

    setAutoWarmupAccountIds((prev) => {
      const next = new Set(Array.from(prev).filter((id) => validAccountIds.has(id)));
      return next.size === prev.size ? prev : next;
    });

    setAutoWarmupLedger((prev) => {
      const next = Object.fromEntries(
        Object.entries(prev).filter(([accountId]) => validAccountIds.has(accountId))
      );
      return Object.keys(next).length === Object.keys(prev).length ? prev : next;
    });

    for (const accountId of Object.keys(autoWarmupRetryAfterRef.current)) {
      if (!validAccountIds.has(accountId)) {
        delete autoWarmupRetryAfterRef.current[accountId];
      }
    }
  }, [accounts, error, loading]);

  useEffect(() => {
    autoWarmupLedgerRef.current = autoWarmupLedger;
    try {
      window.localStorage.setItem(
        AUTO_WARMUP_LEDGER_STORAGE_KEY,
        JSON.stringify(autoWarmupLedger)
      );
    } catch {
      // Ignore storage errors; auto warm-up still works for the current session.
    }
  }, [autoWarmupLedger]);

  useEffect(() => {
    try {
      writeAutoWarmupAllEnabled(autoWarmupAllEnabled);
    } catch {
      // Ignore storage errors; auto warm-up still works for the current session.
    }

    if (isTauriRuntime()) {
      void import("@tauri-apps/api/event")
        .then(({ emit }) => emit(AUTO_WARMUP_ALL_CHANGED_EVENT, autoWarmupAllEnabled))
        .catch((err) => console.error("Failed to sync tray auto warm-up:", err));
    }
  }, [autoWarmupAllEnabled]);

  useEffect(() => {
    try {
      window.localStorage.setItem(
        AUTO_WARMUP_ACCOUNTS_STORAGE_KEY,
        JSON.stringify(Array.from(autoWarmupAccountIds))
      );
    } catch {
      // Ignore storage errors; auto warm-up still works for the current session.
    }
  }, [autoWarmupAccountIds]);

  const handleTitlebarDrag = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      if (!isTauriRuntime() || event.button !== 0) return;
      void appWindow.startDragging();
    },
    []
  );

  const handleTitlebarDoubleClick = useCallback(() => {
    if (!isTauriRuntime()) return;
    void appWindow.toggleMaximize();
  }, []);

  const toggleMask = (accountId: string) => {
    setMaskedAccounts((prev) => {
      const next = new Set(prev);
      if (next.has(accountId)) {
        next.delete(accountId);
      } else {
        next.add(accountId);
      }
      void saveMaskedAccountIds(Array.from(next));
      return next;
    });
  };

  const allMasked =
    accounts.length > 0 && accounts.every((account) => maskedAccounts.has(account.id));

  const toggleMaskAll = () => {
    setMaskedAccounts((prev) => {
      const shouldMaskAll = !accounts.every((account) => prev.has(account.id));
      const next = shouldMaskAll ? new Set(accounts.map((account) => account.id)) : new Set<string>();
      void saveMaskedAccountIds(Array.from(next));
      return next;
    });
  };

  const checkProcesses = useCallback(async () => {
    try {
      const info = await invokeBackend<CodexProcessInfo>("check_codex_processes");
      setProcessInfo((prev) => {
        if (
          prev &&
          prev.can_switch === info.can_switch &&
          prev.count === info.count &&
          prev.background_count === info.background_count &&
          prev.external_count === info.external_count &&
          prev.pids.length === info.pids.length &&
          prev.pids.every((pid, index) => pid === info.pids[index])
        ) {
          return prev;
        }
        return info;
      });
      return info;
    } catch (err) {
      console.error("Failed to check processes:", err);
      return null;
    }
  }, []);

  // Check processes on mount and periodically
  useEffect(() => {
    checkProcesses();
    const interval = setInterval(checkProcesses, 5000);
    return () => clearInterval(interval);
  }, [checkProcesses]);

  // Load masked accounts from storage on mount
  useEffect(() => {
    loadMaskedAccountIds().then((ids) => {
      if (ids.length > 0) {
        setMaskedAccounts(new Set(ids));
      }
    });
  }, [loadMaskedAccountIds]);

  useEffect(() => {
    if (!isActionsMenuOpen) return;

    const handleClickOutside = (event: MouseEvent) => {
      if (!actionsMenuRef.current) return;
      if (!actionsMenuRef.current.contains(event.target as Node)) {
        setIsActionsMenuOpen(false);
      }
    };

    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [isActionsMenuOpen]);

  useEffect(() => {
    if (!isNavMenuOpen) return;

    const handleClickOutside = (event: MouseEvent) => {
      if (!navMenuRef.current) return;
      if (!navMenuRef.current.contains(event.target as Node)) {
        setIsNavMenuOpen(false);
      }
    };

    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [isNavMenuOpen]);

  useEffect(() => {
    if (!isTimedWarmupOpen) return;

    const handleClickOutside = (event: MouseEvent) => {
      if (!navMenuRef.current) return;
      if (!navMenuRef.current.contains(event.target as Node)) {
        setIsTimedWarmupOpen(false);
      }
    };

    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [isTimedWarmupOpen]);

  useEffect(() => {
    applyTheme(themeMode);
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, themeMode);
    } catch {
      // Ignore storage errors; theme still works for current session.
    }

    if (isTauriRuntime()) {
      void import("@tauri-apps/api/event")
        .then(({ emit }) => emit(THEME_CHANGED_EVENT, themeMode))
        .catch((err) => console.error("Failed to sync tray theme:", err));
    }
  }, [themeMode]);

  useEffect(() => {
    if (!isTauriRuntime() || isMacOs) return;

    let unlisten: (() => void) | undefined;

    const syncMaximizedState = async () => {
      try {
        setIsWindowMaximized(await appWindow.isMaximized());
      } catch (err) {
        console.error("Failed to read window state:", err);
      }
    };

    void syncMaximizedState();

    appWindow
      .onResized(() => {
        void syncMaximizedState();
      })
      .then((fn) => {
        unlisten = fn;
      })
      .catch((err) => {
        console.error("Failed to watch window resize:", err);
      });

    return () => {
      unlisten?.();
    };
  }, []);

  const handleSwitch = async (accountId: string) => {
    try {
      setSwitchingId(accountId);
      const latestProcessInfo = await checkProcesses();
      if (!latestProcessInfo) {
        showWarmupToast("无法检查正在运行的 Codex 服务，请重试。", true);
        return;
      }
      if (!latestProcessInfo.can_switch) {
        setPendingSwitchAccountId(accountId);
        setForceCloseConfirmOpen(true);
        return;
      }

      await switchAccount(accountId);
    } catch (err) {
      console.error("Failed to switch account:", err);
      const latestProcessInfo = await checkProcesses();
      if (latestProcessInfo && !latestProcessInfo.can_switch) {
        setPendingSwitchAccountId(accountId);
        setForceCloseConfirmOpen(true);
      } else {
        showWarmupToast(`切换失败： ${formatWarmupError(err)}`, true);
      }
    } finally {
      setSwitchingId(null);
    }
  };

  const handleDelete = async (accountId: string) => {
    if (deleteConfirmId !== accountId) {
      setDeleteConfirmId(accountId);
      setTimeout(() => setDeleteConfirmId(null), 3000);
      return;
    }

    try {
      await deleteAccount(accountId);
      setDeleteConfirmId(null);
    } catch (err) {
      console.error("Failed to delete account:", err);
    }
  };

  const handleRefresh = async () => {
    setIsRefreshing(true);
    setRefreshSuccess(false);
    try {
      await refreshUsage(undefined, { refreshMetadata: true });
      setRefreshSuccess(true);
      setTimeout(() => setRefreshSuccess(false), 2000);
    } finally {
      setIsRefreshing(false);
    }
  };

  const showWarmupToast = useCallback((message: string, isError = false) => {
    if (toastTimerRef.current) clearTimeout(toastTimerRef.current);
    setWarmupToast({ message, isError });
    toastTimerRef.current = setTimeout(() => setWarmupToast(null), isError ? 10000 : 2500);
  }, []);

  const formatWarmupError = useCallback((err: unknown) => {
    if (!err) return "未知错误";
    const message = err instanceof Error ? err.message : typeof err === "string" ? err : null;
    if (message?.startsWith("Cannot switch accounts while ")) {
      return "仍有 Codex 服务在运行，请先在服务面板中处理后再切换。";
    }
    if (message) return message;
    try {
      return JSON.stringify(err);
    } catch {
      return "未知错误";
    }
  }, []);

  const markSuccessfulWarmup = useCallback(
    (accountId: string, timestamp = Date.now(), window?: AutoWarmupWindow) => {
      delete autoWarmupRetryAfterRef.current[accountId];
      setAutoWarmupLedger((prev) => ({
        ...prev,
        [accountId]: {
          lastSuccessfulWarmupAt: timestamp,
          ...(window
            ? {
                lastAutoWindowKey: getAutoWarmupWindowKey(window),
                lastAutoWindowKind: window.kind,
              }
            : {}),
        },
      }));
    },
    []
  );

  const {
    forceCloseConfirmOpen,
    setForceCloseConfirmOpen,
    isForceClosingCodex: isKillingCodex,
    closeCodexProcesses,
  } = useForceCloseCodexProcesses({
    processCount: processInfo?.count ?? 0,
    checkProcesses,
    showToast: showWarmupToast,
    formatError: formatWarmupError,
  });
  const isForceClosingCodex = isKillingCodex || isCompletingForceClose;
  const desktopReopen = useDesktopReopen(forceCloseConfirmOpen);
  const codexClose = useCodexClosePreference(forceCloseConfirmOpen);
  const saveDesktopReopenPreference = (value: DesktopReopenPreference) => {
    try {
      desktopReopen.savePreference(value);
    } catch (err) {
      showWarmupToast(`无法保存偏好设置： ${formatWarmupError(err)}`, true);
    }
  };
  const saveCodexClosePreference = (value: CodexClosePreference) => {
    try {
      codexClose.savePreference(value);
    } catch (err) {
      showWarmupToast(`无法保存关闭偏好： ${formatWarmupError(err)}`, true);
    }
  };


  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let unlistenAutoWarmup: (() => void) | undefined;
    let unlistenCloseBehavior: (() => void) | undefined;

    void (async () => {
      if (!isTauriRuntime()) return;
      const { listen } = await import("@tauri-apps/api/event");
      unlisten = await listen<SwitchAccountBlockedPayload>(
        SWITCH_ACCOUNT_BLOCKED_EVENT,
        async (event) => {
          if (forceCloseInFlightRef.current) return;
          const latestProcessInfo = await checkProcesses();
          const accountId = event.payload?.accountId;

          if (accountId && latestProcessInfo && !latestProcessInfo.can_switch) {
            setPendingSwitchAccountId(accountId);
            setForceCloseConfirmOpen(true);
            return;
          }

          if (accountId && latestProcessInfo?.can_switch) {
            try {
              setSwitchingId(accountId);
              await switchAccount(accountId);
              setPendingSwitchAccountId(null);
              showWarmupToast("已通过菜单栏切换账号。");
            } catch (err) {
              console.error("Failed to retry tray account switch:", err);
              showWarmupToast(`切换失败： ${formatWarmupError(err)}`, true);
            } finally {
              setSwitchingId(null);
            }
            return;
          }

          showWarmupToast(
            event.payload?.error ? formatWarmupError(event.payload.error) : "当前状态不允许切换账号。",
            true
          );
        }
      );
      unlistenAutoWarmup = await listen<boolean>(
        AUTO_WARMUP_ALL_CHANGED_EVENT,
        ({ payload }) => {
          if (typeof payload === "boolean") {
            setAutoWarmupAllEnabled(payload);
          }
        }
      );
      unlistenCloseBehavior = await listen<CloseBehaviorRequestedPayload>(
        CLOSE_BEHAVIOR_REQUESTED_EVENT,
        ({ payload }) => {
          const requestId = payload?.requestId;
          if (typeof requestId === "number") {
            void invokeBackend("ack_close_behavior_prompt", { requestId });
          }
          setCloseBehaviorDontAskAgain(false);
          setCloseBehaviorPromptOpen(true);
        }
      );
    })();

    return () => {
      unlisten?.();
      unlistenAutoWarmup?.();
      unlistenCloseBehavior?.();
    };
  }, [checkProcesses, formatWarmupError, setForceCloseConfirmOpen, showWarmupToast, switchAccount]);

  const handleCloseBehaviorChoice = useCallback(
    async (mode: DockDisplayMode) => {
      try {
        setIsCompletingCloseBehavior(true);
        await invokeBackend("complete_close_behavior", {
          mode,
          dontAskAgain: closeBehaviorDontAskAgain,
        });
        setCloseBehaviorPromptOpen(false);
      } catch (err) {
        console.error("Failed to complete close behavior:", err);
        showWarmupToast(`关闭失败： ${formatWarmupError(err)}`, true);
      } finally {
        setIsCompletingCloseBehavior(false);
      }
    },
    [closeBehaviorDontAskAgain, formatWarmupError, showWarmupToast]
  );

  const handleForceCloseConfirm = async () => {
    if (forceCloseInFlightRef.current || desktopReopen.checking || !canCloseCodexDesktop(processInfo)) return;
    forceCloseInFlightRef.current = true;
    const accountId = pendingSwitchAccountId;
    const shouldReopen = desktopReopen.available && desktopReopen.reopen;
    setIsCompletingForceClose(true);
    try {
      try {
        desktopReopen.rememberSelection();
      } catch (err) {
        showWarmupToast(`无法保存偏好设置： ${formatWarmupError(err)}`, true);
      }
      try {
        codexClose.rememberSelection();
      } catch (err) {
        showWarmupToast(`无法保存关闭偏好： ${formatWarmupError(err)}`, true);
      }
      const result = await closeCodexProcesses(shouldReopen);
      if (!result?.processInfo?.can_switch) return;

      await finishForceClose(
        { canSwitch: true, reopenToken: shouldReopen ? result.reopenToken : null },
        accountId ? async () => {
          setSwitchingId(accountId);
          await switchAccount(accountId);
          showWarmupToast("已关闭 Codex 桌面并切换账号。");
        } : null,
        async (token) => {
          try {
            await invokeBackend("reopen_closed_codex_desktop", { token });
            showWarmupToast(accountId ? "账号已切换，Codex 桌面已重新打开。" : "Codex 桌面已重新打开。");
          } catch (err) {
            showWarmupToast(`Codex 已关闭${accountId ? "，账号已切换" : ""}，但重新打开失败：${formatWarmupError(err)}`, true);
          }
        },
      );
      if (shouldReopen && !result.reopenToken) {
        showWarmupToast("未能识别刚关闭的桌面应用，请手动打开 Codex。", true);
      }
    } catch (err) {
      console.error("Failed to switch account after closing Codex:", err);
      showWarmupToast(`Codex 已关闭，但账号切换失败： ${formatWarmupError(err)}`, true);
    } finally {
      setPendingSwitchAccountId(null);
      setSwitchingId(null);
      setIsCompletingForceClose(false);
      forceCloseInFlightRef.current = false;
      void checkProcesses();
    }
  };

  const handleWarmupAccount = async (accountId: string, accountName: string) => {
    try {
      setWarmingUpId(accountId);
      await warmupAccount(accountId);
      markSuccessfulWarmup(accountId);
      showWarmupToast(`已向 ${accountName} 发送预热请求`);
    } catch (err) {
      console.error("Failed to warm up account:", err);
      showWarmupToast(
        `${accountName} 预热失败：${formatWarmupError(err)}`,
        true
      );
    } finally {
      setWarmingUpId(null);
    }
  };

  const handleWarmupAll = async () => {
    try {
      setIsWarmingAll(true);
      const summary = await warmupAllAccounts();
      if (summary.total_accounts === 0) {
        showWarmupToast("暂无可预热的账号", true);
        return;
      }

      const warmedAt = Date.now();
      const failedAccountIds = new Set(summary.failed_account_ids);
      accounts.forEach((account) => {
        if (!failedAccountIds.has(account.id)) {
          markSuccessfulWarmup(account.id, warmedAt);
        }
      });

      if (summary.failed_account_ids.length === 0) {
        showWarmupToast(
          `已向全部 ${summary.warmed_accounts} 个账号发送预热请求`
        );
      } else {
        showWarmupToast(
          `预热成功 ${summary.warmed_accounts}/${summary.total_accounts} 个，失败 ${summary.failed_account_ids.length} 个`,
          true
        );
      }
    } catch (err) {
      console.error("Failed to warm up all accounts:", err);
      showWarmupToast(`批量预热失败： ${formatWarmupError(err)}`, true);
    } finally {
      setIsWarmingAll(false);
    }
  };

  const toggleAutoWarmupAccount = (accountId: string) => {
    setAutoWarmupAccountIds((prev) => {
      const next = new Set(prev);
      if (next.has(accountId)) {
        next.delete(accountId);
      } else {
        next.add(accountId);
      }
      return next;
    });
  };

  const getDueAutoWarmupForAccount = useCallback(
    (accountId: string, usage: UsageInfo | undefined) => {
      return getDueAutoWarmupWindow(usage, autoWarmupLedgerRef.current[accountId]);
    },
    []
  );

  const formatWindowDuration = (minutes: number | null | undefined): string => {
    if (!minutes || minutes <= 0) return "";
    if (minutes < 24 * 60) {
      return `${Math.ceil(minutes / 60)} 小时`;
    }
    return `${Math.ceil(minutes / (24 * 60))} 天`;
  };

  const getAutoWarmupLabel = useCallback(
    (
      usage: UsageInfo | undefined,
      isEnabled: boolean,
      isRunning: boolean
    ) => {
      if (isRunning) return "正在预热…";
      if (!isEnabled) return "已关闭";
      if (!usage || usage.error) return "已开启";

      const windowKind = getAutoWarmupWindowKind(usage);
      if (windowKind === "session" && isLimitFull(usage.secondary_used_percent)) {
        const weeklyDuration = formatWindowDuration(usage.secondary_window_minutes);
        return weeklyDuration ? `等待 ${weeklyDuration} 额度重置` : "等待额度重置";
      }
      if (windowKind === "session") {
        return formatWindowDuration(usage.primary_window_minutes) || "5 小时";
      }
      if (windowKind === "weekly") {
        return formatWindowDuration(usage.secondary_window_minutes) || "7 天";
      }

      return "已开启";
    },
    []
  );

  const headerAutoWarmupLabel = useMemo(() => {
    if (autoWarmupRunningIds.size > 0) return "正在自动预热…";
    return autoWarmupAllEnabled || autoWarmupAccountIds.size > 0
      ? "自动：已开启"
      : "自动：已关闭";
  }, [autoWarmupAccountIds.size, autoWarmupAllEnabled, autoWarmupRunningIds]);

  const timedWarmupTargetsReady = useMemo(
    () =>
      accounts.length > 0 &&
      accounts.every((account) => account.usage && !account.usageLoading),
    [accounts]
  );

  const timedWarmupTargetCount = useMemo(
    () => getTimedWarmupTargets(accounts).length,
    [accounts]
  );

  const backOffAutoWarmupRetry = useCallback((accountId: string) => {
    autoWarmupRetryAfterRef.current[accountId] =
      Date.now() + AUTO_WARMUP_RETRY_BACKOFF_MS;
  }, []);

  const runAutoWarmupForAccount = useCallback(
    async (accountId: string, accountName: string) => {
      setAutoWarmupRunningIds((prev) => new Set(prev).add(accountId));

      try {
        let freshUsage: UsageInfo;
        try {
          freshUsage = await refreshSingleUsage(accountId);
        } catch (err) {
          console.error("Auto warm-up usage refresh failed:", err);
          backOffAutoWarmupRetry(accountId);
          return;
        }

        const window = getDueAutoWarmupForAccount(accountId, freshUsage);
        if (!window) return;

        await warmupAccount(accountId);
        markSuccessfulWarmup(accountId, Date.now(), window);
        const modeLabel = window.kind === "session" ? "5 小时" : "每周";
        showWarmupToast(`已向 ${accountName} 发送 ${modeLabel} 自动预热请求`);
      } catch (err) {
        console.error("Auto warm-up failed:", err);
        backOffAutoWarmupRetry(accountId);
        showWarmupToast(
          `${accountName} 自动预热失败：${formatWarmupError(err)}`,
          true
        );
      } finally {
        setAutoWarmupRunningIds((prev) => {
          const next = new Set(prev);
          next.delete(accountId);
          return next;
        });
      }
    },
    [
      backOffAutoWarmupRetry,
      formatWarmupError,
      getDueAutoWarmupForAccount,
      markSuccessfulWarmup,
      refreshSingleUsage,
      showWarmupToast,
      warmupAccount,
    ]
  );

  useEffect(() => {
    if (!autoWarmupAllEnabled && autoWarmupAccountIds.size === 0) return;

    const checkAutoWarmup = () => {
      for (const account of accountsRef.current) {
        const autoEnabled =
          autoWarmupAllEnabled || autoWarmupAccountIdsRef.current.has(account.id);
        if (!autoEnabled || autoWarmupRunningIdsRef.current.has(account.id)) continue;

        const retryAfter = autoWarmupRetryAfterRef.current[account.id];
        if (retryAfter && Date.now() < retryAfter) continue;

        if (!getDueAutoWarmupForAccount(account.id, account.usage)) continue;

        void runAutoWarmupForAccount(account.id, account.name);
      }
    };

    checkAutoWarmup();
    const interval = window.setInterval(
      checkAutoWarmup,
      AUTO_WARMUP_CHECK_INTERVAL_MS
    );

    return () => window.clearInterval(interval);
  }, [
    autoWarmupAccountIds.size,
    autoWarmupAllEnabled,
    getDueAutoWarmupForAccount,
    runAutoWarmupForAccount,
  ]);

  const runTimedWarmup = useCallback(async () => {
    const targets = getTimedWarmupTargets(accountsRef.current);
    if (targets.length === 0) return;

    setTimedWarmupRunning(true);
    try {
      const warmedAt = Date.now();
      let warmed = 0;
      let failed = 0;
      for (const account of targets) {
        try {
          await warmupAccount(account.id);
          markSuccessfulWarmup(account.id, warmedAt);
          warmed += 1;
        } catch (err) {
          console.error("定时预热 failed:", err);
          failed += 1;
        }
      }

      if (failed === 0) {
        showWarmupToast(
          `定时预热 sent for ${warmed} account${warmed === 1 ? "" : "s"}`
        );
      } else {
        showWarmupToast(`定时预热: ${warmed} ok, ${failed} failed`, true);
      }
    } finally {
      setTimedWarmupRunning(false);
    }
  }, [markSuccessfulWarmup, showWarmupToast, warmupAccount]);

  useEffect(() => {
    if (!timedWarmupEnabled || timedWarmupTimes.length === 0) return;

    const checkTimedWarmup = () => {
      if (timedWarmupRunningRef.current) return;

      const now = new Date();
      const todayKey = `${now.getFullYear()}-${now.getMonth()}-${now.getDate()}`;
      const currentTime = `${String(now.getHours()).padStart(2, "0")}:${String(
        now.getMinutes()
      ).padStart(2, "0")}`;

      // Only fire during the scheduled minute itself; a missed time (e.g. while
      // asleep) is skipped rather than warmed late at the wrong moment.
      if (!timedWarmupTimes.includes(currentTime)) return;
      if (timedWarmupLastFireRef.current[currentTime] === todayKey) return;
      if (!timedWarmupTargetsReady || timedWarmupTargetCount === 0) return;

      // Mark before running so a slow warm-up can't double-fire on the next tick.
      timedWarmupLastFireRef.current[currentTime] = todayKey;
      try {
        window.localStorage.setItem(
          TIMED_WARMUP_LEDGER_STORAGE_KEY,
          JSON.stringify(timedWarmupLastFireRef.current)
        );
      } catch {
        // Ignore storage errors; timed warm-up still works for the current session.
      }
      void runTimedWarmup();
    };

    checkTimedWarmup();
    const interval = window.setInterval(
      checkTimedWarmup,
      AUTO_WARMUP_CHECK_INTERVAL_MS
    );

    return () => window.clearInterval(interval);
  }, [
    timedWarmupEnabled,
    timedWarmupTimes,
    timedWarmupTargetsReady,
    timedWarmupTargetCount,
    runTimedWarmup,
  ]);

  const handleAddTimedWarmupTime = useCallback(() => {
    const normalized = normalizeTimedWarmupTimes([timedWarmupDraft]);
    if (normalized.length === 0) return;
    setTimedWarmupTimes((prev) =>
      normalizeTimedWarmupTimes([...prev, normalized[0]])
    );
    setTimedWarmupDraft("");
  }, [timedWarmupDraft]);

  const handleRemoveTimedWarmupTime = useCallback((time: string) => {
    setTimedWarmupTimes((prev) => prev.filter((entry) => entry !== time));
  }, []);

  const timedWarmupLabel = useMemo(() => {
    if (timedWarmupRunning) return "正在定时预热…";
    if (!timedWarmupEnabled || timedWarmupTimes.length === 0) return "定时：已关闭";

    const now = new Date();
    const nowMinutes = now.getHours() * 60 + now.getMinutes();
    const upcoming = timedWarmupTimes.find((time) => {
      const [hours, minutes] = time.split(":").map(Number);
      return hours * 60 + minutes > nowMinutes;
    });
    return `定时：${upcoming ?? timedWarmupTimes[0]}`;
  }, [timedWarmupEnabled, timedWarmupRunning, timedWarmupTimes]);

  const handleExportSlimText = async () => {
    setConfigModalMode("slim_export");
    setConfigModalError(null);
    setConfigPayload("");
    setConfigCopied(false);
    setIsConfigModalOpen(true);

    try {
      setIsExportingSlim(true);
      const payload = await exportAccountsSlimText();
      setConfigPayload(payload);
      showWarmupToast(`精简文本已导出（${accounts.length} 个账号）。`);
    } catch (err) {
      console.error("Failed to export slim text:", err);
      const message = err instanceof Error ? err.message : String(err);
      setConfigModalError(message);
      showWarmupToast("精简文本导出失败", true);
    } finally {
      setIsExportingSlim(false);
    }
  };

  const openImportSlimTextModal = () => {
    setConfigModalMode("slim_import");
    setConfigModalError(null);
    setConfigPayload("");
    setConfigCopied(false);
    setIsConfigModalOpen(true);
  };

  const handleImportSlimText = async () => {
    if (!configPayload.trim()) {
      setConfigModalError("请先粘贴精简配置文本。");
      return;
    }

    try {
      setIsImportingSlim(true);
      setConfigModalError(null);
      const summary = await importAccountsSlimText(configPayload);
      setMaskedAccounts(new Set());
      setIsConfigModalOpen(false);
      showWarmupToast(
        `已导入 ${summary.imported_count} 个，跳过 ${summary.skipped_count} 个（共 ${summary.total_in_payload} 个）`
      );
    } catch (err) {
      console.error("Failed to import slim text:", err);
      const message = err instanceof Error ? err.message : String(err);
      setConfigModalError(message);
      showWarmupToast("精简文本导入失败", true);
    } finally {
      setIsImportingSlim(false);
    }
  };

  const handleExportFullFile = async () => {
    try {
      setIsExportingFull(true);
      const exported = await exportFullBackupFile();
      if (!exported) return;
      showWarmupToast("完整加密备份已导出。");
    } catch (err) {
      console.error("Failed to export full encrypted file:", err);
      showWarmupToast("完整备份导出失败", true);
    } finally {
      setIsExportingFull(false);
    }
  };

  const handleImportFullFile = async () => {
    try {
      setIsImportingFull(true);
      const summary = await importFullBackupFile();
      if (!summary) return;
      const accountList = await loadAccounts();
      await refreshUsage(accountList);
      const maskedIds = await loadMaskedAccountIds();
      setMaskedAccounts(new Set(maskedIds));
      showWarmupToast(
        `已导入 ${summary.imported_count} 个，跳过 ${summary.skipped_count} 个（共 ${summary.total_in_payload} 个）`
      );
    } catch (err) {
      console.error("Failed to import full encrypted file:", err);
      showWarmupToast("完整备份导入失败", true);
    } finally {
      setIsImportingFull(false);
    }
  };

  const handleOpenCodexApp = async () => {
    try {
      setIsOpeningCodex(true);
      await invokeBackend("open_codex_app");
      showWarmupToast("已打开 Codex。");
      setTimeout(() => {
        void checkProcesses();
      }, 1500);
    } catch (err) {
      console.error("Failed to open Codex app:", err);
      showWarmupToast(`打开 Codex 失败： ${formatWarmupError(err)}`, true);
    } finally {
      setIsOpeningCodex(false);
    }
  };

  const activeAccount = accounts.find((a) => a.is_active);
  const otherAccounts = accounts.filter((a) => !a.is_active);
  const hasRunningProcesses = processInfo && processInfo.count > 0;
  const canCloseDesktop = canCloseCodexDesktop(processInfo);
  const hasExternalProcesses = (processInfo?.external_count ?? 0) > 0;
  const pendingSwitchAccount = useMemo(
    () => accounts.find((account) => account.id === pendingSwitchAccountId),
    [accounts, pendingSwitchAccountId]
  );
  const closeConfirmLabel = pendingSwitchAccount
    ? "关闭桌面并切换账号"
    : "关闭 Codex 桌面";

  const sortedOtherAccounts = useMemo(() => {
    const getResetDeadline = (resetAt: number | null | undefined) =>
      resetAt ?? Number.POSITIVE_INFINITY;

    const getSubscriptionDeadline = (expiresAt: string | null | undefined) => {
      if (!expiresAt) return null;
      const timestamp = new Date(expiresAt).getTime();
      return Number.isNaN(timestamp) ? null : timestamp;
    };

    const compareOptionalNumber = (
      aValue: number | null,
      bValue: number | null,
      direction: "asc" | "desc"
    ) => {
      if (aValue === null && bValue === null) return 0;
      if (aValue === null) return 1;
      if (bValue === null) return -1;
      return direction === "asc" ? aValue - bValue : bValue - aValue;
    };

    const getRemainingPercent = (usedPercent: number | null | undefined) => {
      if (usedPercent === null || usedPercent === undefined) {
        return Number.NEGATIVE_INFINITY;
      }
      return Math.max(0, 100 - usedPercent);
    };

    return [...otherAccounts].sort((a, b) => {
      if (
        otherAccountsSort === "subscription_asc" ||
        otherAccountsSort === "subscription_desc"
      ) {
        const subscriptionDiff = compareOptionalNumber(
          getSubscriptionDeadline(a.subscription_expires_at),
          getSubscriptionDeadline(b.subscription_expires_at),
          otherAccountsSort === "subscription_asc" ? "asc" : "desc"
        );
        if (subscriptionDiff !== 0) return subscriptionDiff;

        const deadlineDiff =
          getResetDeadline(getPreferredResetsAt(a.usage)) -
          getResetDeadline(getPreferredResetsAt(b.usage));
        if (deadlineDiff !== 0) return deadlineDiff;

        const remainingDiff =
          getRemainingPercent(getPreferredUsedPercent(b.usage)) -
          getRemainingPercent(getPreferredUsedPercent(a.usage));
        if (remainingDiff !== 0) return remainingDiff;

        return a.name.localeCompare(b.name);
      }

      if (otherAccountsSort === "deadline_asc" || otherAccountsSort === "deadline_desc") {
        const deadlineDiff =
          getResetDeadline(getPreferredResetsAt(a.usage)) -
          getResetDeadline(getPreferredResetsAt(b.usage));
        if (deadlineDiff !== 0) {
          return otherAccountsSort === "deadline_asc" ? deadlineDiff : -deadlineDiff;
        }
        const remainingDiff =
          getRemainingPercent(getPreferredUsedPercent(b.usage)) -
          getRemainingPercent(getPreferredUsedPercent(a.usage));
        if (remainingDiff !== 0) return remainingDiff;
        return a.name.localeCompare(b.name);
      }

      const remainingDiff =
        getRemainingPercent(getPreferredUsedPercent(b.usage)) -
        getRemainingPercent(getPreferredUsedPercent(a.usage));
      if (otherAccountsSort === "remaining_desc" && remainingDiff !== 0) {
        return remainingDiff;
      }
      if (otherAccountsSort === "remaining_asc" && remainingDiff !== 0) {
        return -remainingDiff;
      }
      const deadlineDiff =
        getResetDeadline(getPreferredResetsAt(a.usage)) -
        getResetDeadline(getPreferredResetsAt(b.usage));
      if (deadlineDiff !== 0) return deadlineDiff;
      return a.name.localeCompare(b.name);
    });
  }, [otherAccounts, otherAccountsSort]);

  const normalizedAccountSearchQuery = isAccountSearchEnabled
    ? accountSearchQuery.trim().toLowerCase()
    : "";
  const hasMatchingActiveAccount =
    activeAccount !== undefined &&
    matchesAccountSearch(activeAccount, normalizedAccountSearchQuery);
  const visibleOtherAccounts = useMemo(
    () =>
      sortedOtherAccounts.filter((account) =>
        matchesAccountSearch(account, normalizedAccountSearchQuery)
      ),
    [normalizedAccountSearchQuery, sortedOtherAccounts]
  );
  const hasNoMatchingAccounts =
    normalizedAccountSearchQuery.length > 0 &&
    !hasMatchingActiveAccount &&
    visibleOtherAccounts.length === 0;

  return (
    <div className="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-950 dark:text-gray-100">
      <header className="sticky top-0 z-40 border-b border-gray-200 bg-white dark:border-gray-800 dark:bg-gray-900">
        <div className="flex h-9 items-center bg-white px-3 dark:bg-gray-900">
          <div
            onMouseDown={handleTitlebarDrag}
            onDoubleClick={handleTitlebarDoubleClick}
            className={`h-full flex-1 select-none cursor-default ${isMacOs ? "ml-18 mr-2" : "mr-3"}`}
          />
          {!isMacOs && (
            <div className="flex items-center gap-1">
              <button
                onClick={() => {
                  void appWindow.minimize();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-gray-100 hover:text-gray-900 dark:text-gray-400 dark:hover:bg-gray-800 dark:hover:text-gray-100"
                title="最小化窗口"
              >
                <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                  <path d="M5 12h14" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
              <button
                onClick={() => {
                  void appWindow.toggleMaximize();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-gray-100 hover:text-gray-900 dark:text-gray-400 dark:hover:bg-gray-800 dark:hover:text-gray-100"
                title={isWindowMaximized ? "还原窗口" : "最大化窗口"}
              >
                {isWindowMaximized ? (
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                    <path d="M9 9h10v10H9z" strokeWidth="2" />
                    <path d="M5 15V5h10" strokeWidth="2" strokeLinecap="round" />
                  </svg>
                ) : (
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                    <rect x="5" y="5" width="14" height="14" strokeWidth="2" />
                  </svg>
                )}
              </button>
              <button
                onClick={() => {
                  void appWindow.close();
                }}
                className="flex h-8 w-8 items-center justify-center rounded-md text-gray-500 transition-colors hover:bg-red-500 hover:text-white dark:text-gray-400 dark:hover:bg-red-500 dark:hover:text-white"
                title="关闭"
              >
                <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor">
                  <path d="M6 6l12 12M18 6L6 18" strokeWidth="2" strokeLinecap="round" />
                </svg>
              </button>
            </div>
          )}
        </div>

        <div className="max-w-5xl mx-auto px-6 py-4">
          <div className="grid grid-cols-1 gap-3 md:grid-cols-[minmax(0,1fr)_max-content] md:items-center md:gap-4">
            <div className="flex items-center gap-3 min-w-0 flex-1">
              <div className="min-w-0">
                <div className="flex items-center gap-2 flex-wrap">
                  <h1 className="text-xl font-bold text-gray-900 dark:text-gray-100 tracking-tight">
                    Codex Switcher
                  </h1>
                  {processInfo && (
                    <div className="inline-flex items-center gap-1">
                      <span
                        className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-xs border ${hasRunningProcesses
                            ? "bg-amber-50 text-amber-700 border-amber-200 dark:bg-amber-900/30 dark:text-amber-300 dark:border-amber-700"
                            : "bg-green-50 text-green-700 border-green-200 dark:bg-green-900/30 dark:text-green-300 dark:border-green-700"
                          }`}
                      >
                        <span
                          className={`inline-block w-1.5 h-1.5 rounded-full ${hasRunningProcesses ? "bg-amber-500" : "bg-green-500"
                            }`}
                        ></span>
                        <span>
                          {hasRunningProcesses
                            ? `${processInfo.count} 个 Codex 服务运行中`
                            : "无 Codex 服务运行"}
                        </span>
                      </span>
                      {hasRunningProcesses && (
                        <span className="inline-flex" title={hasExternalProcesses ? "独立服务仍在运行，请先在服务面板中处理；此按钮仅正常退出桌面" : "请求 Codex 桌面正常退出，不会强制结束任务"}>
                        <button
                          onClick={() => {
                            setPendingSwitchAccountId(null);
                            setForceCloseConfirmOpen(true);
                          }}
                          disabled={isForceClosingCodex || !canCloseDesktop}
                          className="inline-flex items-center rounded-md border border-red-200 bg-red-50 px-2 py-0.5 text-xs font-medium text-red-700 transition-colors hover:bg-red-100 disabled:opacity-50 dark:border-red-800 dark:bg-red-900/20 dark:text-red-300 dark:hover:bg-red-900/30"
                          title={hasExternalProcesses ? "请先在“运行中的服务”中检查并停止独立会话，再正常退出桌面" : "正常退出 Codex 桌面客户端"}
                        >
                          关闭桌面
                        </button>
                        </span>
                      )}
                    </div>
                  )}
                  {isTauriRuntime() && (
                    <button onClick={() => setIsProcessPanelOpen(true)}
                      title="查看 Codex 相关服务的名称、PID 和路径，并按需逐项停止独立服务"
                      className="rounded-md border border-gray-200 px-2 py-1 text-xs text-gray-700 hover:bg-gray-100 dark:border-gray-700 dark:text-gray-200 dark:hover:bg-gray-800">
                      运行中的服务
                    </button>
                  )}
                  {hasExternalProcesses && (
                    <p className="w-full text-xs text-amber-700 dark:text-amber-300">
                      {processInfo?.external_count} 个独立服务仍在运行。可在“运行中的服务”中查看详情并逐项停止。
                    </p>
                  )}
                  {isTauriRuntime() && processInfo && !hasRunningProcesses && (
                    <button
                      onClick={handleOpenCodexApp}
                      disabled={isOpeningCodex || isCompletingForceClose || switchingId !== null}
                      className="inline-flex items-center rounded-md border border-green-200 bg-green-50 px-2 py-0.5 text-xs font-medium text-green-700 transition-colors hover:bg-green-100 disabled:opacity-50 dark:border-green-800 dark:bg-green-900/20 dark:text-green-300 dark:hover:bg-green-900/30"
                      title="打开 Codex 桌面客户端"
                    >
                      {isOpeningCodex ? "正在打开…" : "打开 Codex"}
                    </button>
                  )}
                </div>
              </div>
            </div>

            <div className="flex flex-wrap items-center gap-2 shrink-0 md:ml-4 md:w-max md:flex-nowrap md:justify-end">
              <button
                onClick={toggleMaskAll}
                aria-label={allMasked ? "显示账号信息" : "隐藏账号信息"}
                className="flex h-10 w-10 items-center justify-center rounded-lg bg-gray-100 text-gray-700 transition-colors hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700 shrink-0"
                title={allMasked ? "显示所有账号的名称和邮箱，便于识别账号" : "隐藏所有账号的名称和邮箱，方便截图或共享屏幕"}
              >
                {allMasked ? (
                  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M13.875 18.825A10.05 10.05 0 0112 19c-4.478 0-8.268-2.943-9.543-7a9.97 9.97 0 011.563-3.029m5.858.908a3 3 0 114.243 4.243M9.878 9.878l4.242 4.242M9.88 9.88l-3.29-3.29m7.532 7.532l3.29 3.29M3 3l3.59 3.59m0 0A9.953 9.953 0 0112 5c4.478 0 8.268 2.943 9.543 7a10.025 10.025 0 01-4.132 5.411m0 0L21 21"
                    />
                  </svg>
                ) : (
                  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M2.458 12C3.732 7.943 7.523 5 12 5c4.478 0 8.268 2.943 9.542 7-1.274 4.057-5.064 7-9.542 7-4.477 0-8.268-2.943-9.542-7z" />
                  </svg>
                )}
              </button>
              <button
                onClick={handleRefresh}
                aria-label="刷新全部用量"
                disabled={isRefreshing}
                className="flex h-10 w-10 items-center justify-center rounded-lg bg-gray-100 text-gray-700 transition-colors hover:bg-gray-200 disabled:opacity-50 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700 shrink-0"
                title={isRefreshing ? "正在更新全部账号的用量和订阅信息" : "刷新全部账号的剩余额度和订阅信息"}
              >
                <span className={isRefreshing ? "animate-spin inline-block" : ""}>↻</span>
              </button>
              <span className="inline-flex" title={accounts.length === 0 ? "先添加账号后才能预热。预热会发送少量测试请求并消耗额度。" : "预热全部账号：发送少量测试请求，会消耗少量额度"}>
              <button
                onClick={() => void handleWarmupAll()}
                aria-label="预热全部账号"
                disabled={isWarmingAll || accounts.length === 0}
                className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors disabled:opacity-50 shrink-0 ${
                  isWarmingAll
                    ? "bg-amber-100 text-amber-500 dark:bg-amber-900/30 dark:text-amber-300"
                    : "bg-amber-50 text-amber-700 hover:bg-amber-100 dark:bg-amber-900/20 dark:text-amber-300 dark:hover:bg-amber-900/40"
                }`}
                title={accounts.length === 0 ? "先添加账号后才能预热。预热会发送少量测试请求并消耗额度。" : isWarmingAll ? "正在向全部账号发送少量测试请求" : "预热全部账号：发送少量测试请求，会消耗少量额度"}
              >
                <span className={isWarmingAll ? "animate-pulse" : ""}>⚡</span>
              </button>
              </span>
              {isAccountSearchEnabled && (
                <button
                  onClick={() => {
                    if (isAccountSearchOpen) {
                      setAccountSearchQuery("");
                    }
                    setIsAccountSearchOpen((prev) => !prev);
                  }}
                  className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors shrink-0 ${
                    isAccountSearchOpen
                      ? "bg-gray-900 text-white hover:bg-gray-800 dark:bg-black dark:text-white dark:hover:bg-neutral-900"
                      : "bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
                  }`}
                  title={isAccountSearchOpen ? "收起账号搜索并清空搜索条件" : "按名称或邮箱搜索账号"}
                >
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <circle cx="11" cy="11" r="7" />
                    <path d="m20 20-3.5-3.5" strokeLinecap="round" />
                  </svg>
                </button>
              )}

              <div className="relative" ref={navMenuRef}>
                <button
                  onClick={() => {
                    setIsTimedWarmupOpen(false);
                    setIsNavMenuOpen((prev) => !prev);
                  }}
                  className={`flex h-10 w-10 items-center justify-center rounded-lg transition-colors shrink-0 ${
                    isNavMenuOpen
                      ? "bg-gray-900 text-white hover:bg-gray-800 dark:bg-black dark:text-white dark:hover:bg-neutral-900"
                      : "bg-gray-100 text-gray-700 hover:bg-gray-200 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
                  }`}
                  title="更多功能：设置、自动预热、定时预热和外观"
                  aria-label="更多功能"
                >
                  <svg className="h-4 w-4" viewBox="0 0 24 24" fill="currentColor">
                    <circle cx="12" cy="5" r="1.6" />
                    <circle cx="12" cy="12" r="1.6" />
                    <circle cx="12" cy="19" r="1.6" />
                  </svg>
                </button>
                {isNavMenuOpen && (
                  <div className="absolute right-0 z-50 mt-2 w-64 rounded-xl border border-gray-200 bg-white p-2 text-gray-700 shadow-xl dark:border-neutral-800 dark:bg-black dark:text-white">
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setIsSettingsOpen(true);
                      }}
                      title="设置正常关闭方式及切换后是否重新打开 Codex"
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      设置
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setAutoWarmupAllEnabled((prev) => !prev);
                      }}
                      title="在额度重置后自动发送少量请求，启动下一轮计时；会消耗额度"
                      disabled={accounts.length === 0}
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>自动预热</span>
                      <span
                        className={`rounded-md px-1.5 py-0.5 text-[11px] font-medium ${
                          autoWarmupAllEnabled
                            ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300"
                            : "bg-gray-100 text-gray-500 dark:bg-gray-800 dark:text-gray-400"
                        }`}
                      >
                        {headerAutoWarmupLabel}
                      </span>
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setIsTimedWarmupOpen((prev) => !prev);
                      }}
                      title="设置每天的预热时间；预热会发送请求并消耗少量额度"
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>定时预热</span>
                      <span
                        className={`rounded-md px-1.5 py-0.5 text-[11px] font-medium ${
                          timedWarmupEnabled
                            ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300"
                            : "bg-gray-100 text-gray-500 dark:bg-gray-800 dark:text-gray-400"
                        }`}
                      >
                        {timedWarmupLabel}
                      </span>
                    </button>
                    <button
                      onClick={() => {
                        setIsNavMenuOpen(false);
                        setThemeMode((prev) => (prev === "dark" ? "light" : "dark"));
                      }}
                      title="在浅色和深色界面之间切换"
                      className="flex w-full items-center justify-between gap-2 rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      <span>外观</span>
                      <span className="text-[11px] text-gray-400 dark:text-gray-500">
                        {themeMode === "dark" ? "☾ 深色" : "☀ 浅色"}
                      </span>
                    </button>
                  </div>
                )}
                {isTimedWarmupOpen && (
                  <div className="absolute right-0 z-20 mt-2 w-64 rounded-lg border border-gray-200 bg-white p-3 shadow-lg dark:border-gray-700 dark:bg-gray-900">
                    <label className="flex items-center justify-between text-sm font-medium text-gray-800 dark:text-gray-100">
                      <span>定时预热</span>
                      <input
                        type="checkbox"
                        checked={timedWarmupEnabled}
                        onChange={(e) => setTimedWarmupEnabled(e.target.checked)}
                        className="h-4 w-4 accent-emerald-600"
                      />
                    </label>
                    <div className="mt-3 space-y-1">
                      {timedWarmupTimes.length === 0 ? (
                        <p className="text-xs italic text-gray-400 dark:text-gray-500">
                          尚未设置预热时间。
                        </p>
                      ) : (
                        timedWarmupTimes.map((time) => (
                          <div
                            key={time}
                            className="flex items-center justify-between rounded-md bg-gray-50 px-2 py-1 text-sm dark:bg-gray-800"
                          >
                            <span className="font-mono text-gray-800 dark:text-gray-100">
                              {time}
                            </span>
                            <button
                              onClick={() => handleRemoveTimedWarmupTime(time)}
                              className="text-gray-400 transition-colors hover:text-red-500"
                              title={`移除 ${time} 的定时预热`}
                            >
                              ✕
                            </button>
                          </div>
                        ))
                      )}
                    </div>

                    <div className="mt-3 flex items-center gap-2">
                      <input
                        type="time"
                        value={timedWarmupDraft}
                        onChange={(e) => setTimedWarmupDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") handleAddTimedWarmupTime();
                        }}
                        className="h-8 flex-1 rounded-md border border-gray-300 bg-white px-2 text-sm text-gray-800 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-100"
                      />
                      <button
                        onClick={handleAddTimedWarmupTime}
                title="将所选时间加入每日自动预热计划，会消耗少量额度"
                        disabled={!timedWarmupDraft}
                        className="h-8 rounded-md bg-gray-900 px-3 text-xs font-semibold text-white transition-colors hover:bg-gray-800 disabled:opacity-50 dark:bg-black dark:hover:bg-neutral-900"
                      >
                        添加
                      </button>
                    </div>
                  </div>
                )}
              </div>
              <div className="relative" ref={actionsMenuRef}>
                <button
                  onClick={() => setIsActionsMenuOpen((prev) => !prev)}
                title="添加账号，或导入、导出账号备份"
                  className="h-10 px-4 py-2 text-sm font-medium rounded-lg bg-gray-900 text-white transition-colors hover:bg-gray-800 dark:bg-black dark:hover:bg-neutral-900 shrink-0 whitespace-nowrap"
                >
                  账号 ▾
                </button>
                {isActionsMenuOpen && (
                  <div className="absolute right-0 z-50 mt-2 w-56 rounded-xl border border-gray-200 bg-white p-2 text-gray-700 shadow-xl dark:border-neutral-800 dark:bg-black dark:text-white">
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        setIsAddModalOpen(true);
                      }}
                      title="添加一个账号，仅保存登录状态，不切换当前账号"
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 dark:text-white dark:hover:bg-neutral-900"
                    >
                      + 添加账号
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleExportSlimText();
                      }}
                      title="将账号凭据导出为精简文本，请勿分享给他人"
                      disabled={isExportingSlim}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isExportingSlim ? "正在导出…" : "导出精简文本"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        openImportSlimTextModal();
                      }}
                      title="粘贴精简备份文本并导入其中未保存的账号"
                      disabled={isImportingSlim}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isImportingSlim ? "正在导入…" : "导入精简文本"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleExportFullFile();
                      }}
                      title="将完整账号信息保存为加密备份文件"
                      disabled={isExportingFull}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isExportingFull ? "正在导出…" : "导出完整加密备份"}
                    </button>
                    <button
                      onClick={() => {
                        setIsActionsMenuOpen(false);
                        void handleImportFullFile();
                      }}
                      title="从完整加密备份文件中恢复账号"
                      disabled={isImportingFull}
                      className="w-full rounded-lg px-3 py-2 text-left text-sm transition-colors hover:bg-gray-100 disabled:opacity-50 dark:text-white dark:hover:bg-neutral-900"
                    >
                      {isImportingFull ? "正在导入…" : "导入完整加密备份"}
                    </button>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </header>

      {/* Main Content */}
      <main className="max-w-5xl mx-auto px-6 pt-4 pb-8">
        {loading && accounts.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-20">
            <div className="animate-spin h-10 w-10 border-2 border-gray-900 dark:border-gray-100 border-t-transparent rounded-full mb-4"></div>
            <p className="text-gray-500 dark:text-gray-400">正在加载账号…</p>
          </div>
        ) : error ? (
          <div className="text-center py-20">
            <div className="text-red-600 dark:text-red-300 mb-2">账号加载失败</div>
            <p className="text-sm text-gray-500 dark:text-gray-400">{error}</p>
          </div>
        ) : accounts.length === 0 ? (
          <div className="text-center py-20">
            <div className="h-16 w-16 rounded-2xl bg-gray-100 dark:bg-gray-800 flex items-center justify-center mx-auto mb-4">
              <span className="text-3xl">👤</span>
            </div>
            <h2 className="text-xl font-semibold text-gray-900 dark:text-gray-100 mb-2">
              还没有添加账号
            </h2>
            <p className="text-gray-500 dark:text-gray-400 mb-6">
              先添加一个 Codex 账号，即可管理和切换
            </p>
            <button
              onClick={() => setIsAddModalOpen(true)}
                title="通过 ChatGPT 授权或登录文件添加账号；添加后不会自动切换"
              className="px-6 py-3 text-sm font-medium rounded-lg bg-gray-900 hover:bg-gray-800 dark:bg-gray-100 dark:hover:bg-gray-200 text-white dark:text-gray-900 transition-colors"
            >
              添加账号
            </button>
          </div>
        ) : (
          <div className="space-y-4">
            {hasNoMatchingAccounts && (
              <div className="rounded-2xl border border-dashed border-gray-300 px-6 py-12 text-center dark:border-gray-700">
                <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">
                  没有找到匹配的账号
                </h2>
                <p className="mt-1 text-sm text-gray-500 dark:text-gray-400">
                  请尝试其他账号名称或邮箱。
                </p>
              </div>
            )}

            {isAccountSearchEnabled && isAccountSearchOpen && (
              <div className="relative w-full">
                <span className="pointer-events-none absolute inset-y-0 left-3 flex items-center text-gray-400 dark:text-gray-500">
                  <svg
                    className="h-4 w-4"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    aria-hidden="true"
                  >
                    <circle cx="11" cy="11" r="7" />
                    <path d="m20 20-3.5-3.5" strokeLinecap="round" />
                  </svg>
                </span>
                <input
                  type="search"
                  value={accountSearchQuery}
                  onChange={(event) => setAccountSearchQuery(event.target.value)}
                  placeholder="输入账号名称或邮箱"
                  aria-label="按名称或邮箱搜索账号"
                  autoFocus
                  className="w-full rounded-xl border border-gray-300 bg-white py-2.5 pl-10 pr-10 text-sm text-gray-900 shadow-sm transition-colors placeholder:text-gray-400 focus:border-gray-400 focus:outline-none focus:ring-2 focus:ring-gray-200 dark:border-gray-700 dark:bg-gray-900 dark:text-gray-100 dark:placeholder:text-gray-500 dark:focus:border-gray-600 dark:focus:ring-gray-800"
                />
                {accountSearchQuery.length > 0 && (
                  <button
                    type="button"
                    onClick={() => setAccountSearchQuery("")}
                    aria-label="清空搜索条件，显示全部账号"
                    title="清空搜索条件，显示全部账号"
                    className="absolute inset-y-0 right-2 flex items-center px-2 text-gray-400 transition-colors hover:text-gray-700 dark:text-gray-500 dark:hover:text-gray-200"
                  >
                    <svg
                      className="h-4 w-4"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      aria-hidden="true"
                    >
                      <path d="m8 8 8 8M16 8l-8 8" strokeLinecap="round" />
                    </svg>
                  </button>
                )}
              </div>
            )}

            {/* 当前账号 */}
            {activeAccount &&
              matchesAccountSearch(activeAccount, normalizedAccountSearchQuery) && (
                <section>
                  <h2 className="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wider mb-4">
                    当前账号
                  </h2>
                  <AccountCard
                    account={activeAccount}
                    onSwitch={() => { }}
                    onWarmup={() =>
                      handleWarmupAccount(activeAccount.id, activeAccount.name)
                    }
                    onDelete={() => handleDelete(activeAccount.id)}
                    onRefresh={() =>
                      refreshSingleUsage(activeAccount.id, { refreshMetadata: true })
                    }
                    onRename={(newName) => renameAccount(activeAccount.id, newName)}
                    switching={switchingId === activeAccount.id}
                    switchDisabled={switchingId !== null || isForceClosingCodex}
                    codexRunning={hasRunningProcesses ?? false}
                    warmingUp={
                      isWarmingAll ||
                      warmingUpId === activeAccount.id ||
                      autoWarmupRunningIds.has(activeAccount.id)
                    }
                    masked={maskedAccounts.has(activeAccount.id)}
                    onToggleMask={() => toggleMask(activeAccount.id)}
                    autoWarmupEnabled={
                      autoWarmupAllEnabled || autoWarmupAccountIds.has(activeAccount.id)
                    }
                    autoWarmupManagedByAll={autoWarmupAllEnabled}
                    autoWarmupLabel={getAutoWarmupLabel(
                      activeAccount.usage,
                      autoWarmupAllEnabled || autoWarmupAccountIds.has(activeAccount.id),
                      autoWarmupRunningIds.has(activeAccount.id)
                    )}
                    onToggleAutoWarmup={() => toggleAutoWarmupAccount(activeAccount.id)}
                  />
                </section>
              )}

            {/* Other Accounts */}
            {visibleOtherAccounts.length > 0 && (
              <section>
                <div className="flex items-center justify-between gap-3 mb-4">
                  <h2 className="text-sm font-medium text-gray-500 dark:text-gray-400 uppercase tracking-wider">
                    其他账号（{
                      normalizedAccountSearchQuery
                        ? `${visibleOtherAccounts.length} / ${otherAccounts.length}`
                        : otherAccounts.length
                    }）
                  </h2>
                  <div className="flex items-center gap-2">
                    <label htmlFor="other-accounts-sort" className="text-xs text-gray-500 dark:text-gray-400">
                      排序
                    </label>
                    <div className="relative">
                      <select
                        id="other-accounts-sort"
                        value={otherAccountsSort}
                        onChange={(e) =>
                          setOtherAccountsSort(
                            e.target.value as
                              | "deadline_asc"
                              | "deadline_desc"
                              | "remaining_desc"
                              | "remaining_asc"
                              | "subscription_asc"
                              | "subscription_desc"
                          )
                        }
                        className="appearance-none font-sans text-xs sm:text-sm font-medium pl-3 pr-9 py-2 rounded-xl border border-gray-300 dark:border-gray-700 bg-gradient-to-b from-white to-gray-50 dark:from-gray-900 dark:to-gray-800 text-gray-700 dark:text-gray-200 shadow-sm hover:border-gray-400 dark:hover:border-gray-600 hover:shadow focus:outline-none focus:ring-2 focus:ring-gray-300 dark:focus:ring-gray-600 focus:border-gray-400 dark:focus:border-gray-600 transition-all"
                      >
                        <option value="deadline_asc">额度重置：由早到晚</option>
                        <option value="deadline_desc">额度重置：由晚到早</option>
                        <option value="remaining_desc">
                          剩余额度：由多到少
                        </option>
                        <option value="remaining_asc">
                          剩余额度：由少到多
                        </option>
                        <option value="subscription_asc">
                          订阅到期：由早到晚
                        </option>
                        <option value="subscription_desc">
                          订阅到期：由晚到早
                        </option>
                      </select>
                      <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-gray-500 dark:text-gray-400">
                        <svg
                          className="h-4 w-4"
                          viewBox="0 0 20 20"
                          fill="none"
                          stroke="currentColor"
                          strokeWidth="2"
                        >
                          <path d="M6 8l4 4 4-4" strokeLinecap="round" strokeLinejoin="round" />
                        </svg>
                      </span>
                    </div>
                  </div>
                </div>
                <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
                  {visibleOtherAccounts.map((account) => (
                    <AccountCard
                      key={account.id}
                      account={account}
                      onSwitch={() => handleSwitch(account.id)}
                      onWarmup={() => handleWarmupAccount(account.id, account.name)}
                      onDelete={() => handleDelete(account.id)}
                      onRefresh={() =>
                        refreshSingleUsage(account.id, { refreshMetadata: true })
                      }
                      onRename={(newName) => renameAccount(account.id, newName)}
                      switching={switchingId === account.id}
                      switchDisabled={switchingId !== null || isForceClosingCodex}
                      codexRunning={hasRunningProcesses ?? false}
                      warmingUp={
                        isWarmingAll ||
                        warmingUpId === account.id ||
                        autoWarmupRunningIds.has(account.id)
                      }
                      masked={maskedAccounts.has(account.id)}
                      onToggleMask={() => toggleMask(account.id)}
                      autoWarmupEnabled={
                        autoWarmupAllEnabled || autoWarmupAccountIds.has(account.id)
                      }
                      autoWarmupManagedByAll={autoWarmupAllEnabled}
                      autoWarmupLabel={getAutoWarmupLabel(
                        account.usage,
                        autoWarmupAllEnabled || autoWarmupAccountIds.has(account.id),
                        autoWarmupRunningIds.has(account.id)
                      )}
                      onToggleAutoWarmup={() => toggleAutoWarmupAccount(account.id)}
                    />
                  ))}
                </div>
              </section>
            )}
          </div>
        )}
      </main>

      {/* Refresh Success Toast */}
      {refreshSuccess && (
        <div className="fixed bottom-6 left-1/2 -translate-x-1/2 px-4 py-3 bg-green-600 text-white rounded-lg shadow-lg text-sm flex items-center gap-2">
          <span>✓</span> 用量信息已更新
        </div>
      )}

      {/* Warm-up Toast */}
      {warmupToast && (
        <div
          className={`fixed bottom-20 left-1/2 -translate-x-1/2 px-4 py-3 rounded-lg shadow-lg text-sm ${
            warmupToast.isError
              ? "bg-red-600 text-white"
              : "bg-amber-100 text-amber-900 border border-amber-300 dark:bg-amber-900/30 dark:text-amber-200 dark:border-amber-700"
          }`}
        >
          {warmupToast.message}
        </div>
      )}

      {/* Delete Confirmation Toast */}
      {deleteConfirmId && (
        <div className="fixed bottom-6 left-1/2 -translate-x-1/2 px-4 py-3 bg-red-600 text-white rounded-lg shadow-lg text-sm">
          再次点击删除，确认移除账号
        </div>
      )}

      {isSettingsOpen && (
        <SettingsModal
          reopenPreference={desktopReopen.preference}
          onReopenPreferenceChange={saveDesktopReopenPreference}
          closePreference={codexClose.preference}
          onClosePreferenceChange={saveCodexClosePreference}
          onClose={() => setIsSettingsOpen(false)}
        />
      )}

      {forceCloseConfirmOpen && (
        <div className="fixed inset-0 bg-black/40 flex items-center justify-center z-50">
          <div className="bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 rounded-2xl w-full max-w-md mx-4 shadow-xl">
            <div className="p-5 border-b border-gray-100 dark:border-gray-800">
              <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-100">
                关闭 Codex 桌面客户端？
              </h2>
            </div>
            <div className="p-5 space-y-3">
              <p className="text-sm text-gray-600 dark:text-gray-300">
                请先确认桌面内的任务已结束。工具会请求正常退出，等待客户端完全关闭后才切换账号。
              </p>
              {!canCloseDesktop && (
                <p role="alert" className="text-sm text-amber-700 dark:text-amber-300">
                  {hasExternalProcesses
                    ? `检测到 ${processInfo?.external_count} 个独立 CLI、后台服务或 IDE 会话。请先在“运行中的服务”中检查并逐项停止，再重试切换。`
                    : "尚未确认可安全关闭的 Codex 桌面客户端，请取消并刷新状态后重试。"}
                </p>
              )}
              <div className="space-y-2 rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-800">
                {codexClose.preference !== "ask" ? (
                  <p className="text-sm text-gray-500 dark:text-gray-400">
                    仅正常退出桌面客户端，不提供强制关闭。
                  </p>
                ) : (
                  <>
                    <label className="flex items-center gap-2 text-sm text-gray-500 dark:text-gray-400">
                      <input type="checkbox" checked={codexClose.remember} onChange={(event) => codexClose.setRemember(event.target.checked)} disabled={isForceClosingCodex} className="h-4 w-4 accent-orange-600" />
                      记住本次选择
                    </label>
                    <p className="text-xs text-gray-500 dark:text-gray-400">
                      仅正常退出桌面客户端，不提供强制关闭。
                    </p>
                  </>
                )}
              </div>
              {pendingSwitchAccount && (
                <p className="text-sm text-gray-600 dark:text-gray-300">
                  关闭 Codex 后，将切换到{" "}
                  <span className="font-medium text-gray-900 dark:text-gray-100">
                    {pendingSwitchAccount.name}
                  </span>
                  .
                </p>
              )}
              <div className="space-y-2 rounded-lg bg-gray-50 dark:bg-gray-800 p-3">
                {desktopReopen.checking ? (
                  <p className="text-sm text-gray-500 dark:text-gray-400">正在检查可重新打开的桌面应用…</p>
                ) : desktopReopen.available && desktopReopen.preference !== "ask" ? (
                  <p className="text-sm text-gray-500 dark:text-gray-400">
                    {desktopReopen.preference === "always"
                      ? "Codex 桌面将自动重新打开。"
                      : "Codex 桌面将保持关闭。"}{" "}
                    可在设置中修改。
                  </p>
                ) : desktopReopen.available ? (
                  <>
                    <label className="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-200">
                      <input type="checkbox" checked={desktopReopen.reopen} onChange={(event) => desktopReopen.setReopen(event.target.checked)} disabled={isForceClosingCodex} className="h-4 w-4 accent-orange-600" />
                      关闭后重新打开 Codex 桌面
                    </label>
                    <label className="flex items-center gap-2 text-sm text-gray-500 dark:text-gray-400">
                      <input type="checkbox" checked={desktopReopen.remember} onChange={(event) => desktopReopen.setRemember(event.target.checked)} disabled={isForceClosingCodex} className="h-4 w-4 accent-orange-600" />
                      记住本次选择
                    </label>
                    <p className="text-xs text-gray-500 dark:text-gray-400">可稍后在设置中修改。独立 CLI、后台服务和 IDE 会话需单独停止。</p>
                  </>
                ) : (
                  <p className="text-sm text-gray-500 dark:text-gray-400">未识别到支持的桌面应用，无法自动重新打开。</p>
                )}
              </div>
            </div>
            <div className="flex justify-end gap-3 p-5 border-t border-gray-100 dark:border-gray-800">
              <button
                onClick={() => {
                  setPendingSwitchAccountId(null);
                  setForceCloseConfirmOpen(false);
                }}
                title="取消关闭，保留当前桌面和登录状态"
                disabled={isForceClosingCodex}
                className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-100 hover:bg-gray-200 dark:bg-gray-800 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 transition-colors disabled:opacity-50"
              >
                取消
              </button>
              <button
                onClick={() => {
                  void handleForceCloseConfirm();
                }}
                title="请求 Codex 桌面正常退出，确认退出后才继续切换"
                disabled={isForceClosingCodex || desktopReopen.checking || !canCloseDesktop}
                className="px-4 py-2.5 text-sm font-medium rounded-lg text-white transition-colors disabled:opacity-50 bg-orange-600 hover:bg-orange-700"
              >
                {isForceClosingCodex
                  ? "正在关闭桌面…"
                  : closeConfirmLabel}
              </button>
            </div>
          </div>
        </div>
      )}

      {closeBehaviorPromptOpen && (
        <div className="fixed inset-0 bg-black/40 flex items-center justify-center z-50">
          <div className="bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 rounded-2xl w-full max-w-md mx-4 shadow-xl">
            <div className="p-5 border-b border-gray-100 dark:border-gray-800">
              <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-100">
                关闭窗口后保留 Dock 图标？
              </h2>
            </div>
            <div className="p-5 space-y-4">
              <p className="text-sm text-gray-600 dark:text-gray-300">
                关闭窗口后，可保留 Dock 图标，或仅在菜单栏中运行。
              </p>
              <p className="text-sm text-gray-600 dark:text-gray-300">
                可随时从菜单栏面板中修改。
              </p>
              <label className="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-200">
                <input
                  type="checkbox"
                  checked={closeBehaviorDontAskAgain}
                  onChange={(event) => setCloseBehaviorDontAskAgain(event.target.checked)}
                  className="h-4 w-4 accent-gray-900 dark:accent-gray-100"
                />
                <span>不再询问</span>
              </label>
            </div>
            <div className="flex flex-col gap-2 p-5 border-t border-gray-100 dark:border-gray-800 sm:flex-row sm:justify-end">
              <button
                onClick={() => setCloseBehaviorPromptOpen(false)}
                title="取消本次关闭窗口操作"
                disabled={isCompletingCloseBehavior}
                className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-100 hover:bg-gray-200 dark:bg-gray-800 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 transition-colors disabled:opacity-50"
              >
                取消
              </button>
              <button
                onClick={() => void handleCloseBehaviorChoice("show_in_dock")}
                title="关闭窗口后保留 Dock 图标，应用继续运行"
                disabled={isCompletingCloseBehavior}
                className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-100 hover:bg-gray-200 dark:bg-gray-800 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 transition-colors disabled:opacity-50"
              >
                保留 Dock 图标
              </button>
              <button
                onClick={() => void handleCloseBehaviorChoice("menu_bar_only")}
                title="关闭窗口后仅在顶部菜单栏运行"
                disabled={isCompletingCloseBehavior}
                className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-900 hover:bg-gray-800 dark:bg-gray-100 dark:hover:bg-gray-200 text-white dark:text-gray-900 transition-colors disabled:opacity-50"
              >
                仅在菜单栏运行
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 添加账号 Modal */}
      <AddAccountModal
        isOpen={isAddModalOpen}
        onClose={() => setIsAddModalOpen(false)}
        onImportFile={importFromFile}
        onStartOAuth={startOAuthLogin}
        onCompleteOAuth={completeOAuthLogin}
        onCancelOAuth={cancelOAuthLogin}
      />

      {/* Import/Export Config Modal */}
      {isConfigModalOpen && (
        <div className="fixed inset-0 bg-black/40 flex items-center justify-center z-50">
          <div className="bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 rounded-2xl w-full max-w-2xl mx-4 shadow-xl">
            <div className="flex items-center justify-between p-5 border-b border-gray-100 dark:border-gray-800">
              <h2 className="text-lg font-semibold text-gray-900 dark:text-gray-100">
                {configModalMode === "slim_export" ? "导出精简文本" : "导入精简文本"}
              </h2>
              <button
                onClick={() => setIsConfigModalOpen(false)}
                title="关闭导入或导出窗口"
                className="text-gray-400 hover:text-gray-600 dark:hover:text-gray-300 transition-colors"
              >
                ✕
              </button>
            </div>
            <div className="p-5 space-y-4">
              {configModalMode === "slim_import" ? (
                <p className="text-sm text-amber-700 dark:text-amber-200 bg-amber-50 dark:bg-amber-900/30 border border-amber-200 dark:border-amber-700 rounded-lg px-3 py-2">
                  保留现有账号，仅导入尚未添加的账号。
                </p>
              ) : (
                <p className="text-sm text-gray-500 dark:text-gray-400">
                  精简文本包含账号登录凭据，请妥善保管，不要分享。
                </p>
              )}
              <textarea
                value={configPayload}
                onChange={(e) => setConfigPayload(e.target.value)}
                readOnly={configModalMode === "slim_export"}
                placeholder={
                  configModalMode === "slim_export"
                    ? isExportingSlim
                      ? "正在生成…"
                      : "导出的文本将显示在这里"
                    : "在这里粘贴配置文本"
                }
                className="w-full h-48 px-4 py-3 bg-gray-50 dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-lg text-sm text-gray-800 dark:text-gray-100 placeholder-gray-400 dark:placeholder-gray-500 focus:outline-none focus:border-gray-400 dark:focus:border-gray-500 focus:ring-1 focus:ring-gray-400 dark:focus:ring-gray-500 font-mono"
              />
              {configModalError && (
                <div className="p-3 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-700 rounded-lg text-red-600 dark:text-red-300 text-sm">
                  {configModalError}
                </div>
              )}
            </div>
            <div className="flex gap-3 p-5 border-t border-gray-100 dark:border-gray-800">
              <button
                onClick={() => setIsConfigModalOpen(false)}
                title="关闭导入或导出窗口"
                className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-100 hover:bg-gray-200 dark:bg-gray-800 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 transition-colors"
              >
                关闭
              </button>
              {configModalMode === "slim_export" ? (
                <button
                  onClick={async () => {
                    if (!configPayload) return;
                    try {
                      await navigator.clipboard.writeText(configPayload);
                      setConfigCopied(true);
                      setTimeout(() => setConfigCopied(false), 1500);
                    } catch {
                      setConfigModalError("无法访问剪贴板，请手动复制。");
                    }
                  }}
                  title="复制包含登录凭据的备份文本，请勿分享给他人"
                  disabled={!configPayload || isExportingSlim}
                  className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-900 hover:bg-gray-800 dark:bg-gray-100 dark:hover:bg-gray-200 text-white dark:text-gray-900 transition-colors disabled:opacity-50"
                >
                  {configCopied ? "已复制" : "复制文本"}
                </button>
              ) : (
                <button
                  onClick={handleImportSlimText}
                title="导入文本中尚未保存的账号，保留现有账号"
                  disabled={isImportingSlim}
                  className="px-4 py-2.5 text-sm font-medium rounded-lg bg-gray-900 hover:bg-gray-800 dark:bg-gray-100 dark:hover:bg-gray-200 text-white dark:text-gray-900 transition-colors disabled:opacity-50"
                >
                  {isImportingSlim ? "正在导入…" : "导入尚未添加的账号"}
                </button>
              )}
            </div>
          </div>
        </div>
      )}
      {isProcessPanelOpen && <ProcessPanel onClose={() => setIsProcessPanelOpen(false)} onProcessesChanged={async () => { await checkProcesses(); }} />}
      <HoverHelp />
      <UpdateChecker />

    </div>
  );
}

export default App;
