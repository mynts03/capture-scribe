import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { HistoryWindow } from "./HistoryWindow";
import { AutomationHelpWindow } from "./AutomationHelpWindow";
import "./App.css";
import { applyUIColors } from "./uiColors";

applyUIColors(document.documentElement);
const params = new URLSearchParams(window.location.search);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {params.get("window") === "automation-help" ? <AutomationHelpWindow /> : (params.get("window") === "history" ? <HistoryWindow /> : <App />)}
  </StrictMode>,
);

