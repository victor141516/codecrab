export function registerServiceWorker(browser, logger = console) {
  const serviceWorker = browser?.navigator?.serviceWorker;
  if (!serviceWorker || typeof browser.addEventListener !== "function") return;

  browser.addEventListener("load", () => {
    void serviceWorker.register("/service-worker.js").catch((error) => {
      logger.warn("CodeCrab service worker registration failed", error);
    });
  });
}
