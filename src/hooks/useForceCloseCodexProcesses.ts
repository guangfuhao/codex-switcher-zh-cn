import { useCallback, useState } from "react";
import type { CodexProcessInfo } from "../types";
import { invokeBackend } from "../lib/platform";
import { canCloseCodexDesktop } from "../lib/codexClosePreference";

interface KillCodexProcessesResult {
  targeted_count: number;
  killed_pids: number[];
  failed_pids: number[];
  reopen_token?: string | null;
}

interface UseForceCloseCodexProcessesOptions {
  processCount: number;
  checkProcesses: () => Promise<CodexProcessInfo | null>;
  showToast: (message: string, isError?: boolean) => void;
  formatError: (err: unknown) => string;
}

export function useForceCloseCodexProcesses({
  processCount,
  checkProcesses,
  showToast,
  formatError,
}: UseForceCloseCodexProcessesOptions) {
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [isForceClosing, setIsForceClosing] = useState(false);

  const closeCodexProcesses = useCallback(async (reopenDesktop = false) => {
    try {
      setIsForceClosing(true);

      const beforeClose = await checkProcesses();
      if (!canCloseCodexDesktop(beforeClose)) {
        showToast(
          beforeClose && beforeClose.external_count > 0
            ? "独立 CLI、后台服务或 IDE 会话仍在运行，请自行结束后再切换。工具不会关闭这些会话。"
            : "无法确认可安全关闭的 Codex 桌面客户端，请刷新状态后重试。",
          true,
        );
        return null;
      }

      const result = await invokeBackend<KillCodexProcessesResult>(
        "kill_codex_processes",
        { reopenDesktop, forceClose: false }
      );
      const latestProcessInfo = await checkProcesses();
      const remainingCount = latestProcessInfo?.count ?? processCount;
      const closedCount = Math.max(0, processCount - remainingCount);

      if (!latestProcessInfo) {
        showToast("无法确认 Codex 已关闭，已取消切换账号和重新打开。", true);
      } else if (result.targeted_count === 0) {
        showToast("未发现正在运行的 Codex 服务。");
      } else if (remainingCount === 0) {
        showToast(
          `已关闭 ${processCount} 个 Codex 桌面会话。`
        );
      } else if (closedCount > 0) {
        showToast(
          `已关闭 ${closedCount}/${processCount} 个 Codex 桌面会话，仍有 ${remainingCount} 个运行中。`,
          true
        );
      } else {
        showToast(
          `未能正常关闭 ${remainingCount} 个 Codex 桌面会话。`,
          true
        );
      }

      return { processInfo: latestProcessInfo, reopenToken: reopenDesktop ? result.reopen_token ?? null : null };
    } catch (err) {
      console.error("Failed to close Codex processes:", err);
      showToast(`关闭失败： ${formatError(err)}`, true);
      return null;
    } finally {
      setConfirmOpen(false);
      setIsForceClosing(false);
    }
  }, [checkProcesses, formatError, processCount, showToast]);

  return {
    forceCloseConfirmOpen: confirmOpen,
    setForceCloseConfirmOpen: setConfirmOpen,
    isForceClosingCodex: isForceClosing,
    closeCodexProcesses,
  };
}
