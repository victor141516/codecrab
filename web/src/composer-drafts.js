export const COMPOSER_DRAFT_NAMESPACE = "codecrab:composer-draft:v1";

export function normalizeProjectIdentity(project) {
  const normalized = String(project ?? "")
    .replaceAll("\\", "/")
    .replace(/\/+$/, "");
  const rooted = normalized || "/";
  return /^[A-Z]:/.test(rooted)
    ? rooted[0].toLowerCase() + rooted.slice(1)
    : rooted;
}

export function composerDraftKey(project, sessionId) {
  return [
    COMPOSER_DRAFT_NAMESPACE,
    encodeURIComponent(normalizeProjectIdentity(project)),
    encodeURIComponent(String(sessionId ?? ""))
  ].join(":");
}

export function createComposerDraftStore(storage = defaultStorage()) {
  const cache = new Map();

  function read(project, sessionId) {
    if (!sessionId) return "";
    const key = composerDraftKey(project, sessionId);
    if (cache.has(key)) return cache.get(key) ?? "";
    try {
      const value = storage?.getItem(key);
      cache.set(key, value);
      return value ?? "";
    } catch {
      cache.set(key, null);
      return "";
    }
  }

  function write(project, sessionId, value) {
    if (!sessionId) return;
    const key = composerDraftKey(project, sessionId);
    const text = String(value);
    cache.set(key, text || null);
    try {
      if (text) storage?.setItem(key, text);
      else storage?.removeItem(key);
    } catch {
      // The in-memory cache remains authoritative for this page.
    }
  }

  function remove(project, sessionId) {
    if (!sessionId) return;
    const key = composerDraftKey(project, sessionId);
    cache.set(key, null);
    try {
      storage?.removeItem(key);
    } catch {
      // Navigation and composing must continue when storage is unavailable.
    }
  }

  function removeIfMatches(project, sessionId, value) {
    if (read(project, sessionId) !== value) return false;
    remove(project, sessionId);
    return true;
  }

  return { read, write, remove, removeIfMatches };
}

export function createComposerDraftController({
  store,
  getDraft,
  setDraft,
  afterUpdate,
  resize,
  focus
}) {
  let identity = null;
  let replacing = false;
  let revision = 0;

  async function replaceDraft(value) {
    replacing = true;
    setDraft(value);
    replacing = false;
    await afterUpdate();
    resize();
  }

  function persist(value = getDraft()) {
    if (!replacing && identity) {
      revision += 1;
      store.write(identity.project, identity.sessionId, value);
    }
  }

  async function activate(project, sessionId, { focusComposer = false } = {}) {
    identity = sessionId ? { project, sessionId } : null;
    const value = identity ? store.read(project, sessionId) : "";
    await replaceDraft(value);
    if (focusComposer) focus();
    return value;
  }

  async function beginNavigation() {
    const source = identity
      ? { identity: { ...identity }, value: getDraft() }
      : { identity: null, value: getDraft() };
    persist(source.value);
    identity = null;
    await replaceDraft("");
    return source;
  }

  async function rollbackNavigation(source) {
    identity = source.identity ? { ...source.identity } : null;
    await replaceDraft(source.value);
  }

  async function clear({ keepStored = false } = {}) {
    const clearedIdentity = identity ? { ...identity } : null;
    await replaceDraft("");
    if (!keepStored && clearedIdentity) {
      store.remove(clearedIdentity.project, clearedIdentity.sessionId);
    }
  }

  function snapshot() {
    return {
      identity: identity ? { ...identity } : null,
      value: getDraft(),
      revision
    };
  }

  async function finishSend(sent, succeeded) {
    if (!sent.identity) return;
    const { project, sessionId } = sent.identity;
    if (succeeded) {
      if (revision === sent.revision) {
        store.removeIfMatches(project, sessionId, sent.value);
      }
      return;
    }
    if (
      revision === sent.revision &&
      identity?.project === project &&
      identity?.sessionId === sessionId &&
      !getDraft()
    ) {
      await replaceDraft(sent.value);
      persist(sent.value);
    }
  }

  function forget(project, sessionId) {
    store.remove(project, sessionId);
  }

  return {
    activate,
    beginNavigation,
    rollbackNavigation,
    clear,
    finishSend,
    forget,
    persist,
    snapshot
  };
}

function defaultStorage() {
  try {
    return globalThis.localStorage;
  } catch {
    return null;
  }
}
