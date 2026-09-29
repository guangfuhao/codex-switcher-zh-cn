import { useCallback, useEffect, useRef, useState } from "react";
import { invokeBackend } from "../lib/platform";

type CodexProcessKind = "desktop" | "cli" | "app_server" | "daemon" | "ide";

interface CodexProcessDetail {
  pid: number;
  parent_pid: number | null;
  name: string;
  kind: CodexProcessKind;
  executable_path: string;
  can_stop: boolean;
  stop_disabled_reason: string | null;
  identity: string | null;
}

interface StopCodexProcessResult {
  pid: number;
  stopped: boolean;
  still_running: boolean;
  message: string;
}

interface StopConfirmation {
  pid: number;
  name: string;
  identity: string;
}

interface ProcessPanelProps {
  onClose: () => void;
  onProcessesChanged: () => void | Promise<void>;
}

const kindLabels: Record<CodexProcessKind, string> = {
  desktop: "桌面客户端",
  cli: "命令行会话",
  app_server: "应用服务",
  daemon: "后台服务",
  ide: "IDE 会话",
};

function disabledReason(process: CodexProcessDetail): string | null {
  if (process.kind === "desktop") {
    return "请使用主界面的“关闭”入口正常退出 Codex 桌面客户端。";
  }
  if (!process.can_stop) {
    return process.stop_disabled_reason || "此进程不可由本工具停止，请在所属应用中结束。";
  }
  if (!process.identity) {
    return "尚未取得有效的进程身份，请刷新后重试。";
  }
  return null;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : typeof error === "string" ? error : "发生未知错误";
}

export function ProcessPanel({ onClose, onProcessesChanged }: ProcessPanelProps) {
  const [processes, setProcesses] = useState<CodexProcessDetail[]>([]);
  const [hasLoaded, setHasLoaded] = useState(false);
  const [isRefreshing, setIsRefreshing] = useState(true);
  const [isSynchronizing, setIsSynchronizing] = useState(false);
  const [stoppingPid, setStoppingPid] = useState<number | null>(null);
  const [confirmation, setConfirmation] = useState<StopConfirmation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const requestId = useRef(0);
  const stopInFlight = useRef(false);
  const onProcessesChangedRef = useRef(onProcessesChanged);
  onProcessesChangedRef.current = onProcessesChanged;
  const busy = isRefreshing || isSynchronizing || stoppingPid !== null;

  const refreshProcesses = useCallback(async (clearError = true) => {
    const currentRequest = ++requestId.current;
    setIsRefreshing(true);
    // A refreshed list can contain new identity tokens, even for the same PID.
    setConfirmation(null);
    if (clearError) setError(null);
    try {
      const result = await invokeBackend<CodexProcessDetail[]>("list_codex_process_details");
      if (currentRequest !== requestId.current) return;
      setProcesses(result);
      setHasLoaded(true);
    } catch (err) {
      if (currentRequest !== requestId.current) return;
      const message = `刷新进程列表失败：${errorMessage(err)}`;
      setError((previous) => !clearError && previous ? `${previous}；${message}` : message);
    } finally {
      if (currentRequest === requestId.current) setIsRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void refreshProcesses();
    return () => { requestId.current += 1; };
  }, [refreshProcesses]);

  const updateParentCount = async () => {
    try {
      await onProcessesChangedRef.current();
    } catch (err) {
      const message = `刷新主界面进程数量失败：${errorMessage(err)}`;
      setError((previous) => previous ? `${previous}；${message}` : message);
    }
  };

  const handleRefresh = async () => {
    if (busy || stopInFlight.current) return;
    setNotice(null);
    setIsSynchronizing(true);
    try {
      await refreshProcesses();
      await updateParentCount();
    } finally {
      setIsSynchronizing(false);
    }
  };

  const confirmStop = async () => {
    if (!confirmation || busy || stopInFlight.current) return;
    const target = confirmation;
    const current = processes.find((process) => process.pid === target.pid);
    if (!current || current.identity !== target.identity || disabledReason(current)) {
      setConfirmation(null);
      setError("进程状态已改变，请刷新列表后重新确认停止目标。");
      return;
    }

    stopInFlight.current = true;
    setStoppingPid(target.pid);
    setError(null);
    setNotice(null);
    try {
      const result = await invokeBackend<StopCodexProcessResult>("stop_codex_process", {
        pid: target.pid,
        identity: target.identity,
      });
      if (result.pid === target.pid && result.stopped && !result.still_running) {
        setNotice(`${target.name}（PID ${target.pid}）已停止。`);
      } else {
        setError(result.message || `${target.name}（PID ${target.pid}）仍在运行，请在所属应用中结束后刷新。`);
      }
    } catch (err) {
      setError(`停止 ${target.name}（PID ${target.pid}）失败：${errorMessage(err)}。可刷新列表后重试。`);
    } finally {
      setConfirmation(null);
      await refreshProcesses(false);
      await updateParentCount();
      setStoppingPid(null);
      stopInFlight.current = false;
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4">
      <section
        role="dialog"
        aria-modal="true"
        aria-labelledby="codex-process-panel-title"
        aria-describedby="codex-process-panel-description"
        aria-busy={busy}
        className="flex max-h-[85vh] w-full max-w-2xl flex-col overflow-hidden rounded-2xl border border-gray-200 bg-white shadow-xl dark:border-gray-700 dark:bg-gray-900"
      >
        <header className="flex items-start justify-between gap-4 border-b border-gray-100 p-5 dark:border-gray-800">
          <div>
            <h2 id="codex-process-panel-title" className="text-lg font-semibold text-gray-900 dark:text-gray-100">Codex 相关进程</h2>
            <p id="codex-process-panel-description" className="mt-1 text-sm text-gray-500 dark:text-gray-400">
              仅显示已识别的 Codex 相关进程。停止前请确认对应任务已结束。
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            aria-label="关闭进程面板"
            title={busy ? "正在处理进程，请稍候" : "关闭进程面板，不停止任何进程"}
            className="rounded-md px-2 py-1 text-gray-500 hover:bg-gray-100 disabled:opacity-40 dark:text-gray-400 dark:hover:bg-gray-800"
          >✕</button>
        </header>

        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto p-5">
          <p className="text-xs leading-relaxed text-gray-500 dark:text-gray-400">
            “停止”只向选中的进程发送正常终止请求（SIGTERM），不会强制杀死进程。桌面客户端请使用主界面的“关闭”入口。
          </p>
          {error && <p role="alert" className="rounded-lg border border-red-200 bg-red-50 p-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-900/20 dark:text-red-300">{error}</p>}
          {notice && <p role="status" className="rounded-lg border border-green-200 bg-green-50 p-3 text-sm text-green-700 dark:border-green-900 dark:bg-green-900/20 dark:text-green-300">{notice}</p>}
          {!hasLoaded && isRefreshing && <p role="status" className="py-6 text-center text-sm text-gray-500 dark:text-gray-400">正在读取进程列表…</p>}
          {hasLoaded && processes.length === 0 && !isRefreshing && <p className="py-6 text-center text-sm text-gray-500 dark:text-gray-400">未发现正在运行的 Codex 相关进程。</p>}
          {processes.map((process) => {
            const reason = disabledReason(process);
            const confirming = confirmation?.pid === process.pid && confirmation.identity === process.identity;
            return (
              <article key={process.pid} className="rounded-xl border border-gray-200 p-4 dark:border-gray-700">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <h3 className="break-words text-sm font-semibold text-gray-900 dark:text-gray-100">{process.name}</h3>
                    <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">
                      {kindLabels[process.kind] ?? process.kind} · PID {process.pid} · 父 PID {process.parent_pid ?? "未知"}
                    </p>
                  </div>
                  <button
                    type="button"
                    disabled={busy || reason !== null || confirming}
                    onClick={() => {
                      if (reason || !process.identity) return;
                      setError(null);
                      setNotice(null);
                      setConfirmation({ pid: process.pid, name: process.name, identity: process.identity });
                    }}
                    aria-label={`停止 ${process.name}，PID ${process.pid}`}
                    title={reason ?? `请求正常停止 ${process.name}（PID ${process.pid}），操作前需要确认`}
                    className="shrink-0 rounded-lg border border-orange-200 bg-orange-50 px-3 py-1.5 text-sm font-medium text-orange-700 hover:bg-orange-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-orange-900 dark:bg-orange-900/20 dark:text-orange-300 dark:hover:bg-orange-900/30"
                  >{stoppingPid === process.pid ? "停止中…" : "停止"}</button>
                </div>
                <p className="mt-3 break-all rounded-lg bg-gray-50 px-3 py-2 font-mono text-xs leading-relaxed text-gray-600 dark:bg-gray-800 dark:text-gray-300" title="可执行文件路径">
                  {process.executable_path || "路径不可用"}
                </p>
                {reason && <p className="mt-2 text-xs leading-relaxed text-gray-500 dark:text-gray-400">{reason}</p>}
                {confirming && (
                  <div className="mt-3 space-y-3 rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-900 dark:bg-amber-900/20">
                    <p className="text-sm leading-relaxed text-amber-900 dark:text-amber-200">
                      确认停止 {confirmation.name}（PID {confirmation.pid}）？该进程正在执行的任务会中断。只发送 SIGTERM，不会强制杀死。
                    </p>
                    <div className="flex flex-wrap justify-end gap-2">
                      <button type="button" onClick={() => setConfirmation(null)} disabled={busy} aria-label={`取消停止 PID ${process.pid}`} title="取消，不改变此进程" className="rounded-lg bg-white px-3 py-1.5 text-sm text-gray-700 hover:bg-gray-100 disabled:opacity-40 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700">取消</button>
                      <button type="button" onClick={() => void confirmStop()} disabled={busy} aria-label={`确认停止 ${process.name}，PID ${process.pid}`} title={`向 PID ${process.pid} 发送 SIGTERM，该进程中的任务会中断`} className="rounded-lg bg-orange-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-orange-700 disabled:opacity-40">{stoppingPid === process.pid ? "正在停止…" : "确认停止"}</button>
                    </div>
                  </div>
                )}
              </article>
            );
          })}
        </div>

        <footer className="flex items-center justify-between gap-3 border-t border-gray-100 p-5 dark:border-gray-800">
          <span className="text-xs text-gray-500 dark:text-gray-400">{hasLoaded ? `共 ${processes.length} 个相关进程` : "尚未取得进程列表"}</span>
          <div className="flex gap-2">
            <button type="button" onClick={() => void handleRefresh()} disabled={busy} aria-label="刷新 Codex 相关进程列表" title="重新读取进程状态；已有停止确认会清除" className="rounded-lg bg-gray-100 px-4 py-2 text-sm font-medium text-gray-700 hover:bg-gray-200 disabled:opacity-40 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700">{isRefreshing ? "刷新中…" : "刷新列表"}</button>
            <button type="button" onClick={onClose} disabled={busy} aria-label="完成并关闭进程面板" title={busy ? "正在处理进程，请稍候" : "关闭面板，不停止任何进程"} className="rounded-lg bg-gray-900 px-4 py-2 text-sm font-medium text-white hover:bg-gray-800 disabled:opacity-40 dark:bg-gray-100 dark:text-gray-900 dark:hover:bg-gray-200">完成</button>
          </div>
        </footer>
      </section>
    </div>
  );
}
