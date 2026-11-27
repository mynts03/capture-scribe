import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { translate, type Locale } from "./i18n";

const commands = [
  ["selection-status", "automationHelp.command.selectionStatus"],
  ["cancel-selection", "automationHelp.command.cancelSelection"],
  ["target-displays", "automationHelp.command.targetDisplays"],
  ["target-set", "automationHelp.command.targetSet"],
  ["target-status", "automationHelp.command.targetStatus"],
  ["target-clear", "automationHelp.command.targetClear"],
  ["capture-status", "automationHelp.command.captureStatus"],
  ["--help", "automationHelp.command.help"],
] as const;

function readLanguage(): Locale {
  const { captureScribeHelpLanguage } = window as Window & { captureScribeHelpLanguage?: Locale };
  return captureScribeHelpLanguage === "en" ? "en" : "ja";
}

function closeWindow() {
  void invoke("close_automation_help").catch(() => {});
}

export function AutomationHelpWindow() {
  const [language, setLanguage] = useState(readLanguage);
  useEffect(() => {
    const updateLanguage = () => setLanguage(readLanguage());
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented && !event.isComposing && !event.repeat && document.hasFocus()) {
        event.preventDefault();
        closeWindow();
      }
    };
    window.addEventListener("capturescribe:help-language", updateLanguage);
    window.addEventListener("keydown", closeOnEscape);
    updateLanguage();
    return () => {
      window.removeEventListener("capturescribe:help-language", updateLanguage);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, []);
  useEffect(() => {
    document.documentElement.lang = language;
  }, [language]);

  return (
    <main className="automation-help" aria-labelledby="automation-help-title">
      <h1 id="automation-help-title">{translate(language, "automationHelp.title")}</h1>
      <p>{translate(language, "automationHelp.body")}</p>
      <p>
        {translate(language, "automationHelp.pathLabel")}<br />
        <code>Contents/MacOS/capture-scribe-control</code>
      </p>
      <section className="panel automation-help-commands" aria-labelledby="automation-help-cli">
        <h2 id="automation-help-cli">{translate(language, "automationHelp.cliTitle")}</h2>
        <dl>
          {commands.map(([name, description]) => (
            <div key={name}>
              <dt><code>{name}</code></dt>
              <dd>{translate(language, description)}</dd>
            </div>
          ))}
        </dl>
      </section>
      <p>{translate(language, "automationHelp.access")}</p>
      <p>{translate(language, "automationHelp.limits")}</p>
      <div className="automation-help-actions">
        <button className="secondary" type="button" onClick={closeWindow}>{translate(language, "automationHelp.close")}</button>
      </div>
    </main>
  );
}
