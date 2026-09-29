// Local fixes must not be silently replaced by an upstream release.
export function UpdateChecker() {
  return (
    <div className="px-4 py-2 text-center text-xs text-gray-500 dark:text-gray-400">
      中文修正版 0.2.20-local.2 · 自动更新已关闭
    </div>
  );
}
