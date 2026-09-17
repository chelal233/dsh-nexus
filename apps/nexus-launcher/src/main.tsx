import { ConfirmationHost } from "./confirmation";
import React from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { I18nProvider } from "./i18n";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <I18nProvider>
      <App />
      <ConfirmationHost />
    </I18nProvider>
  </React.StrictMode>,
);
