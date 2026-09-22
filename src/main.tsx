import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { TrayPopup } from "./components/TrayPopup";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { invoke } from "@tauri-apps/api/core";
import { installAppLocale } from "./i18n";

const label = getCurrentWebviewWindow().label;

async function startApp() {
  const systemLocale = await invoke<string | null>("get_system_locale").catch(() => null);
  const appLocale = installAppLocale(systemLocale ?? undefined);
  await invoke("set_app_locale", { locale: appLocale }).catch((error) => {
    console.error("Failed to synchronize the application locale", error);
  });

  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      {label === "tray-popup" ? <TrayPopup /> : <App />}
    </React.StrictMode>,
  );
}

void startApp();
