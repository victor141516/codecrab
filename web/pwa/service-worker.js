const CACHE_NAME = "codecrab-app-shell-v2";
const APP_SHELL = [
  "/",
  "/app.js",
  "/app.css",
  "/manifest.webmanifest",
  "/icon-32.png",
  "/icon-192.png",
  "/icon-512.png"
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE_NAME).then((cache) => cache.addAll(APP_SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((name) => name !== CACHE_NAME).map((name) => caches.delete(name)))
      )
  );
  self.clients.claim();
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (
    request.method !== "GET" ||
    url.origin !== self.location.origin ||
    url.pathname.startsWith("/api/") ||
    url.pathname.startsWith("/code-server/")
  ) {
    return;
  }

  event.respondWith(
    fetch(request)
      .then((response) => {
        if (response.ok && (request.mode === "navigate" || APP_SHELL.includes(url.pathname))) {
          const cachedResponse = response.clone();
          void caches.open(CACHE_NAME).then((cache) => cache.put(request, cachedResponse));
        }
        return response;
      })
      .catch(async () => {
        const cached = await caches.match(request);
        if (cached) return cached;
        if (request.mode === "navigate") {
          const appShell = await caches.match("/");
          if (appShell) return appShell;
        }
        return Response.error();
      })
  );
});
