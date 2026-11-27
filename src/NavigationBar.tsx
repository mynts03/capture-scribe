import { useEffect, useId, useRef, useState } from "react";
import { cardNavigationItems } from "./cardNavigation";
import { translate, type Locale } from "./i18n";

export function CardNavigation({ language, navigate }: { language: Locale; navigate: (id: string) => void }) {
  const id = useId();
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [tip, setTip] = useState<{ index: number; left: number; top: number } | null>(null);
  function hide() {
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = null;
    setTip(null);
  }
  function schedule(index: number, target: HTMLElement) {
    hide();
    timer.current = setTimeout(() => {
      const rect = target.getBoundingClientRect();
      const width = Math.min(320, window.innerWidth - 24);
      setTip({ index, left: Math.max(12, Math.min(rect.left, window.innerWidth - width - 12)), top: rect.bottom + 6 });
    }, 500);
  }
  useEffect(() => {
    window.addEventListener("resize", hide);
    window.addEventListener("scroll", hide, true);
    return () => {
      if (timer.current !== null) clearTimeout(timer.current);
      window.removeEventListener("resize", hide);
      window.removeEventListener("scroll", hide, true);
    };
  }, []);
  return <nav className="card-navigation" aria-label={translate(language, "cardNavigation")}>
    {cardNavigationItems.map(({ headingId, labelKey }, index) => <a key={headingId} href={`#${headingId}`}
      aria-describedby={tip?.index === index ? id : undefined}
      onMouseEnter={event => schedule(index, event.currentTarget)} onMouseLeave={hide}
      onFocus={event => schedule(index, event.currentTarget)} onBlur={hide}
      onKeyDown={event => { if (event.key === "Escape") { hide(); event.preventDefault(); } }}
      onClick={event => { event.preventDefault(); hide(); navigate(headingId); }}>
      {translate(language, labelKey)}
    </a>)}
    {tip && <span id={id} role="tooltip" className="navigation-tooltip" style={{ left: tip.left, top: tip.top }}>
      {translate(language, cardNavigationItems[tip.index].descriptionKey)}
    </span>}
  </nav>;
}
