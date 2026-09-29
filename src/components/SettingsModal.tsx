import { useCallback, useEffect, useRef, useState } from "react";
import type { DesktopReopenPreference } from "../lib/desktopReopen";
import type { CodexClosePreference } from "../lib/codexClosePreference";
import { invokeBackend, isTauriRuntime } from "../lib/platform";
import type { DockDisplayMode } from "../types";

type TrayDisplayMode = "icon_and_session" | "active_usage_text" | "hidden";
interface DisplaySettings {
  tray_display_mode: TrayDisplayMode;
  dock_display_mode: DockDisplayMode | null;
}

interface SettingsModalProps {
  reopenPreference: DesktopReopenPreference;
  onReopenPreferenceChange: (value: DesktopReopenPreference) => void;
  closePreference: CodexClosePreference;
  onClosePreferenceChange: (value: CodexClosePreference) => void;
  onClose: () => void;
}

export function SettingsModal({
  reopenPreference,
  onReopenPreferenceChange,
  closePreference,
  onClosePreferenceChange,
  onClose,
}: SettingsModalProps) {
  const [displaySettings, setDisplaySettings] = useState<DisplaySettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestId = useRef(0);
  const desktop = isTauriRuntime();
  const loadDisplaySettings = useCallback(async () => {
    const currentRequest = ++requestId.current;
    try {
      const settings = await invokeBackend<DisplaySettings>("get_display_settings");
      if (currentRequest === requestId.current) {
        setDisplaySettings(settings);
        setError(null);
      }
    } catch (err) {
      if (currentRequest === requestId.current) setError(String(err));
    }
  }, []);

  useEffect(() => {
    if (!desktop) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const stop = await listen("app-settings-changed", () => {
        void loadDisplaySettings();
      });
      if (disposed) stop();
      else {
        unlisten = stop;
        void loadDisplaySettings();
      }
    }).catch((err) => {
      if (!disposed) setError(String(err));
    });
    return () => {
      disposed = true;
      requestId.current += 1;
      unlisten?.();
    };
  }, [desktop, loadDisplaySettings]);

  const changeDisplaySetting = async (command: string, mode: string) => {
    setSaving(true);
    setError(null);
    try {
      await invokeBackend(command, { mode });
      // Changing tray visibility can also adjust the Dock mode, and vice versa.
      await loadDisplaySettings();
    } catch (err) {
      requestId.current += 1;
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  const selectClassName = "w-full rounded-lg border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-sm text-gray-900 dark:text-gray-100 disabled:opacity-50 disabled:pointer-events-none";

  return (
    <div className="fixed inset-0 bg-black/40 flex items-center justify-center z-50">
      <div role="dialog" aria-modal="true" aria-labelledby="settings-title" className="flex max-h-[calc(100dvh-2rem)] flex-col bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 rounded-2xl w-full max-w-md mx-4 shadow-xl">
        <div className="shrink-0 p-5 border-b border-gray-100 dark:border-gray-800">
          <h2 id="settings-title" className="text-lg font-semibold text-gray-900 dark:text-gray-100">设置</h2>
          <p className="mt-1 text-sm text-gray-500 dark:text-gray-400">界面语言：简体中文</p>
        </div>
        <div className="min-h-0 flex-1 p-5 space-y-3 overflow-y-auto">
          {desktop && (
            <>
              {displaySettings ? (
                <>
                  <label htmlFor="tray-display-mode" className="block text-sm font-medium text-gray-900 dark:text-gray-100">菜单栏显示</label>
                  <div title={saving ? "正在保存显示设置，请稍候" : "选择 Mac 菜单栏中的显示内容，也可隐藏此入口"}>
                  <select
                    id="tray-display-mode"
                    value={displaySettings.tray_display_mode}
                    disabled={saving}
                    onChange={(event) => void changeDisplaySetting("set_tray_display_mode", event.target.value)}
                    className={selectClassName}
                    title="选择菜单栏显示图标、短周期与每周剩余额度，或隐藏入口"
                  >
                    <option value="icon_and_session">图标与短周期剩余额度</option>
                    <option value="active_usage_text">短周期与每周剩余额度</option>
                    <option value="hidden">隐藏</option>
                  </select>
                  </div>
                  <p className="text-xs text-gray-500 dark:text-gray-400">控制屏幕顶部菜单栏的显示内容。短周期额度通常按 5 小时计算，实际周期以账号返回的数据为准；每周额度单独显示。</p>
                  {displaySettings.dock_display_mode !== null && (
                    <>
                      <label htmlFor="dock-display-mode" className="block text-sm font-medium text-gray-900 dark:text-gray-100">程序坞图标</label>
                      <div title={saving ? "正在保存显示设置，请稍候" : "设置 Codex Switcher 是否在程序坞显示图标"}>
                      <select
                        id="dock-display-mode"
                        value={displaySettings.dock_display_mode}
                        disabled={saving}
                        onChange={(event) => void changeDisplaySetting("set_dock_display_mode", event.target.value)}
                        className={selectClassName}
                        title="显示程序坞图标便于打开窗口；仅菜单栏模式可减少程序坞占用"
                      >
                        <option value="show_in_dock">在程序坞中显示</option>
                        <option value="menu_bar_only">仅显示在菜单栏</option>
                      </select>
                      </div>
                      <p className="text-xs text-gray-500 dark:text-gray-400">设置本工具是否出现在 Mac 程序坞中。程序坞与菜单栏至少保留一个入口，方便重新打开 Codex Switcher。</p>
                    </>
                  )}
                </>
              ) : !error && <p className="text-sm text-gray-500 dark:text-gray-400">正在读取显示设置…</p>}
              {error && <p role="alert" className="text-sm text-red-600 dark:text-red-300">无法更新显示设置： {error}</p>}
              <div className="border-t border-gray-100 dark:border-gray-800" />
            </>
          )}
          <label htmlFor="codex-close-preference" className="block text-sm font-medium text-gray-900 dark:text-gray-100">
            Codex 退出方式
          </label>
          <select id="codex-close-preference" value={closePreference} onChange={(event) => onClosePreferenceChange(event.target.value as CodexClosePreference)} className={selectClassName} title="设置退出确认时是否显示方式选择；执行退出前始终需要确认">
            <option value="ask">每次询问</option>
            <option value="graceful">正常退出</option>
          </select>
          <p className="text-sm text-gray-500 dark:text-gray-400">
            两种选项都只请求 Codex 桌面正常退出，执行前仍需确认。“每次询问”会显示方式选择，“正常退出”使用已保存方式。独立 CLI、后台服务和 IDE 会话不会自动结束，可到“运行中的服务”逐项确认停止。
          </p>
          <label htmlFor="desktop-reopen-preference" className="block text-sm font-medium text-gray-900 dark:text-gray-100">
            退出后重新打开 Codex
          </label>
          <select id="desktop-reopen-preference" value={reopenPreference} onChange={(event) => onReopenPreferenceChange(event.target.value as DesktopReopenPreference)} className={selectClassName} title="选择本工具退出 Codex 桌面后是否重新打开；切换账号时会等到切换成功">
            <option value="ask">每次询问</option>
            <option value="always">重新打开桌面客户端</option>
            <option value="never">保持关闭</option>
          </select>
          <p className="text-sm text-gray-500 dark:text-gray-400">
            仅控制本工具关闭的 Codex 桌面客户端。“重新打开”会恢复桌面窗口，“保持关闭”则留待手动打开；切换账号时，仅在切换成功后重开。
          </p>
        </div>
        <div className="flex shrink-0 justify-end p-5 border-t border-gray-100 dark:border-gray-800">
          <span className="inline-flex" title={saving ? "正在保存设置，请稍候" : "关闭设置面板；修改的选项会自动保存"}>
            <button onClick={onClose} disabled={saving} title="关闭设置面板；修改的选项会自动保存" className="px-4 py-2 text-sm font-medium rounded-lg bg-gray-100 dark:bg-gray-800 hover:bg-gray-200 dark:hover:bg-gray-700 text-gray-700 dark:text-gray-200 disabled:opacity-50 disabled:pointer-events-none">完成</button>
          </span>
        </div>
      </div>
    </div>
  );
}
