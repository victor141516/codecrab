import { createApp } from "vue";
import App from "./App.vue";
import { registerServiceWorker } from "./pwa.js";
import "highlight.js/styles/github-dark-dimmed.css";
import "./style.css";

createApp(App).mount("#app");
registerServiceWorker(window);
