import { useEffect, useState } from "react";
import { createPortal } from "react-dom";

/** Also reads wrapper titles so disabled controls still explain their purpose. */
export function HoverHelp() {
  const [help, setHelp] = useState<{ text: string; left: number; top: number; above: boolean } | null>(null);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let active: Element | null = null;
    const hide = () => {
      clearTimeout(timer);
      active = null;
      setHelp(null);
    };
    const show = (event: Event) => {
      const target = event.target instanceof Element ? event.target.closest("[title]") : null;
      if (target === active) return;
      hide();
      const text = target?.getAttribute("title")?.trim();
      if (!target || !text) return;
      active = target;
      timer = setTimeout(() => {
        if (!target.isConnected) return;
        const rect = target.getBoundingClientRect();
        const halfWidth = Math.min(160, (window.innerWidth - 24) / 2);
        setHelp({
          text,
          left: Math.max(halfWidth + 12, Math.min(window.innerWidth - halfWidth - 12, rect.left + rect.width / 2)),
          top: rect.bottom + 8 > window.innerHeight - 110 ? rect.top - 8 : rect.bottom + 8,
          above: rect.bottom + 8 > window.innerHeight - 110,
        });
      }, 250);
    };
    const keydown = (event: KeyboardEvent) => { if (event.key === "Escape") hide(); };
    document.addEventListener("pointerover", show, true);
    document.addEventListener("focusin", show, true);
    document.addEventListener("focusout", hide, true);
    document.addEventListener("pointerdown", hide, true);
    document.addEventListener("scroll", hide, true);
    document.addEventListener("keydown", keydown);
    document.documentElement.addEventListener("pointerleave", hide);
    window.addEventListener("blur", hide);
    window.addEventListener("resize", hide);
    return () => {
      clearTimeout(timer);
      document.removeEventListener("pointerover", show, true);
      document.removeEventListener("focusin", show, true);
      document.removeEventListener("focusout", hide, true);
      document.removeEventListener("pointerdown", hide, true);
      document.removeEventListener("scroll", hide, true);
      document.removeEventListener("keydown", keydown);
      document.documentElement.removeEventListener("pointerleave", hide);
      window.removeEventListener("blur", hide);
      window.removeEventListener("resize", hide);
    };
  }, []);

  return help ? createPortal(
    <div role="tooltip" className="pointer-events-none fixed z-[1000] w-max max-w-[min(320px,calc(100vw-24px))] rounded-lg bg-gray-900 px-3 py-2 text-xs leading-relaxed text-white shadow-lg dark:bg-gray-100 dark:text-gray-900"
      style={{ left: help.left, top: help.top, transform: `translate(-50%, ${help.above ? "-100%" : "0"})` }}>
      {help.text}
    </div>, document.body,
  ) : null;
}
