export const DESKTOP_SIDEBAR_STORAGE_KEY = "codecrab:sidebar-collapsed";

export function loadDesktopSidebarCollapsed(storage = globalThis.localStorage) {
  try {
    return storage?.getItem(DESKTOP_SIDEBAR_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

export function saveDesktopSidebarCollapsed(
  collapsed,
  storage = globalThis.localStorage
) {
  try {
    storage?.setItem(DESKTOP_SIDEBAR_STORAGE_KEY, String(collapsed));
  } catch {
    // Storage failures do not block sidebar interaction.
  }
}
