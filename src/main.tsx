import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "yet-another-react-lightbox/styles.css";
import "yet-another-react-lightbox/plugins/counter.css";
import App from "./App";
import "./styles/globals.css";

const container = document.getElementById("root");
if (!container) throw new Error("missing #root");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
