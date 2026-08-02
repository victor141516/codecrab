<script setup>
import {
  computed,
  nextTick,
  onBeforeUnmount,
  onMounted,
  reactive,
  ref,
  watch
} from "vue";
import {
  ArrowUp,
  CalendarClock,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Copy,
  Folder,
  FolderOpen,
  FolderPlus,
  GitBranch,
  ListTodo,
  LoaderCircle,
  Menu,
  Mic,
  Settings,
  Pause,
  Paperclip,
  Pencil,
  Play,
  Plus,
  Square,
  Trash2,
  X
} from "@lucide/vue";
import { renderMarkdown } from "./markdown.js";
import {
  createComposerDraftController,
  createComposerDraftStore,
  composerDraftKey
} from "./composer-drafts.js";
import {
  bindingsFromMessage,
  insertAttachmentReference,
  reconcileAttachmentBindings,
  trimAttachmentDraft
} from "./attachment-bindings.js";
import { recalledMessageForComposer } from "./composer-recall.js";
import {
  consumeCompletionNdjson,
  mergeCompletionUpdate
} from "./completion-updates.js";
import { isScrolledToBottom } from "./scroll.js";
import {
  insertTranscriptAtSelection,
  isDictationShortcut
} from "./dictation.js";
import {
  acceptsRevision,
  consumeNdjson,
  localStreamEndState,
  liveTurnState,
  mergeCatalog
} from "./session-live.js";
import {
  activityEventTimestamp,
  formatEventTimestamp
} from "./timestamps.js";
import { matchesMessageNode, sortChronologically } from "./timeline.js";
import {
  VIRTUAL_TIMELINE_OVERSCAN,
  VirtualTimelineStore
} from "./virtual-timeline.js";
import {
  branchEdgePath,
  latestActiveBranchNodeId,
  layoutBranchGraph,
  projectEditedSession,
  routeContainsEdge
} from "./branches.js";
import {
  createPromptQueue,
  enqueuePrompt,
  removeQueuedPrompt,
  steerQueuedPrompt as prioritizeQueuedPrompt,
  takeNextQueuedPrompt,
  updateQueuedPrompt
} from "./prompt-queue.js";
import {
  expandKnownProject,
  newSessionPayload,
  toggleProjectExpansion,
  toggleSessionExpansion,
  visibleSessionRows
} from "./project-sidebar.js";

const state = ref(null);
const draft = ref("");
const draftAttachments = ref([]);
const attachmentInput = ref(null);
const uploadingAttachments = ref(false);
const loading = ref(true);
const recording = ref(false);
const transcribing = ref(false);
const error = ref("");
const sidebarOpen = ref(false);
const composer = ref(null);
const conversation = ref(null);
const autoScroll = ref(true);
const waveformCanvas = ref(null);
const autocomplete = ref(null);
const autocompleteSelection = ref(0);
const expandedProjects = ref(new Set());
const collapsedSessions = ref(new Set());
const projectPickerOpen = ref(false);
const directoryListing = ref(null);
const directoryPathDraft = ref("");
const newDirectoryName = ref("");
const projectPickerLoading = ref(false);
const projectPickerError = ref("");
const copiedMessageKey = ref("");
const expandedActivityKeys = ref(new Set());
const overflowingActivityKeys = ref(new Set());
const expandedTurnKeys = ref(new Set());
const goalsOpen = ref(false);
const providersOpen = ref(false);
const cronOpen = ref(false);
const cronDraft = ref(null);
const cronDraftId = ref("");
const cronDraftOriginalId = ref("");
let cronRefreshTimer = null;
const providerDraft = ref(null);
const describedGoal = ref(null);
const editingGoal = ref(null);
const goalDraft = ref("");
const editingGoalResume = ref(false);
const branchesOpen = ref(false);
const selectedBranchNode = ref(null);
const hoveredBranchNode = ref(null);
const branchSelecting = ref(false);
const branchOriginalPath = ref(new Set());
const editingMessageNode = ref(null);
const recalledMessageNode = ref(null);
const messageEditDraft = ref("");
const messageEditAttachments = ref([]);
const messageEditor = ref(null);
let autocompleteRequest = 0;
let autocompleteController = null;
let copiedMessageTimer = null;
const activityDetailElements = new Map();
const activityDetailObservers = new Map();
const timelineRowElements = new Map();
const timelineElementKeys = new Map();
const virtualTimelineStore = new VirtualTimelineStore();
const virtualWindow = ref({
  start: 0,
  end: 0,
  top: 0,
  bottom: 0,
  total: 0
});
const TIMELINE_VERTICAL_PADDING = 32;
let timelineResizeObserver = null;
let conversationResizeObserver = null;
let conversationWidth = 0;
let virtualActivation = 0;
let branchHoverRequest = 0;
let mediaRecorder = null;
let microphoneStream = null;
let recordingChunks = [];
let discardRecording = false;
let recordingStopping = false;
let sendAfterTranscription = false;
let audioContext = null;
let microphoneSource = null;
let audioAnalyser = null;
let waveformFrame = null;
let waveformData = null;
let lastEscapeAt = 0;
let branchOriginalState = null;
let branchOriginalScrollTop = 0;
let branchOriginalAutoScroll = true;
const sessionStates = new Map();
const sessionRuntimes = new Map();
const liveObservationRevisions = new Map();
let liveRevision = 0;
let sessionStreamController = null;
let sessionStreamRetry = null;
let sessionStreamRetryDelay = 250;
const composerDraftStore = createComposerDraftStore();
const composerDrafts = createComposerDraftController({
  store: composerDraftStore,
  getDraft: () => draft.value,
  setDraft: (value) => {
    draft.value = value;
  },
  afterUpdate: nextTick,
  resize: resizeComposer,
  focus: () => composer.value?.focus()
});
let attachmentDraftIdentity = null;
let replacingAttachmentDraft = false;

function attachmentStorageKey(project, sessionId) {
  return `${composerDraftKey(project, sessionId)}:attachments`;
}

function persistAttachmentDraft() {
  if (!attachmentDraftIdentity || replacingAttachmentDraft) return;
  const { project, sessionId } = attachmentDraftIdentity;
  try {
    const key = attachmentStorageKey(project, sessionId);
    if (draftAttachments.value.length) {
      localStorage.setItem(key, JSON.stringify(draftAttachments.value));
    } else {
      localStorage.removeItem(key);
    }
  } catch {
    // Storage failures must not block composing or navigation.
  }
}

function loadAttachmentDraft(project, sessionId) {
  replacingAttachmentDraft = true;
  attachmentDraftIdentity = sessionId ? { project, sessionId } : null;
  try {
    draftAttachments.value = sessionId
      ? JSON.parse(localStorage.getItem(attachmentStorageKey(project, sessionId)) || "[]")
      : [];
  } catch {
    draftAttachments.value = [];
  }
  replacingAttachmentDraft = false;
}

async function activateComposerDraft(project, sessionId, options) {
  await composerDrafts.activate(project, sessionId, options);
  loadAttachmentDraft(project, sessionId);
}

async function beginComposerNavigation() {
  persistAttachmentDraft();
  const attachmentSource = {
    identity: attachmentDraftIdentity ? { ...attachmentDraftIdentity } : null,
    bindings: draftAttachments.value.map((binding) => ({ ...binding }))
  };
  attachmentDraftIdentity = null;
  replacingAttachmentDraft = true;
  draftAttachments.value = [];
  replacingAttachmentDraft = false;
  return { text: await composerDrafts.beginNavigation(), attachmentSource };
}

async function rollbackComposerNavigation(source) {
  await composerDrafts.rollbackNavigation(source.text);
  replacingAttachmentDraft = true;
  attachmentDraftIdentity = source.attachmentSource.identity;
  draftAttachments.value = source.attachmentSource.bindings;
  replacingAttachmentDraft = false;
}

function forgetComposerDraft(project, sessionId) {
  composerDrafts.forget(project, sessionId);
  try {
    localStorage.removeItem(attachmentStorageKey(project, sessionId));
  } catch {
    // Deletion continues when browser storage is unavailable.
  }
}

watch(
  draft,
  (value, previous) => {
    if (!replacingAttachmentDraft) {
      draftAttachments.value = reconcileAttachmentBindings(
        previous ?? "",
        value,
        draftAttachments.value
      );
    }
    composerDrafts.persist(value);
    persistAttachmentDraft();
  },
  { flush: "sync" }
);
watch(draftAttachments, persistAttachmentDraft, { deep: true, flush: "sync" });
watch(messageEditDraft, (value, previous) => {
  messageEditAttachments.value = reconcileAttachmentBindings(
    previous ?? "",
    value,
    messageEditAttachments.value
  );
});

const session = computed(() => state.value?.session ?? null);
function runtimeFor(id) {
  if (!id) {
    return {
      sending: false,
      localStreaming: false,
      liveLifecycle: "",
      cancelling: false,
      promptQueue: createPromptQueue(),
      queuedPromptEdit: null,
      resumeGoalAfterQueue: false,
      pendingGoalAction: null,
      error: ""
    };
  }
  if (!sessionRuntimes.has(id)) {
    sessionRuntimes.set(id, reactive({
      sending: false,
      localStreaming: false,
      liveLifecycle: "",
      cancelling: false,
      promptQueue: createPromptQueue(),
      queuedPromptEdit: null,
      resumeGoalAfterQueue: false,
      pendingGoalAction: null,
      error: ""
    }));
  }
  return sessionRuntimes.get(id);
}
const selectedRuntime = computed(() => runtimeFor(session.value?.id));
const sending = computed({
  get: () => selectedRuntime.value.sending,
  set: (value) => {
    selectedRuntime.value.sending = value;
  }
});
const cancelling = computed({
  get: () => selectedRuntime.value.cancelling,
  set: (value) => {
    selectedRuntime.value.cancelling = value;
  }
});
const queuedPrompts = computed(() => selectedRuntime.value.promptQueue.items);
const projects = computed(() => state.value?.projects ?? []);
const models = computed(() => state.value?.models ?? []);
const dictationAvailable = computed(
  () => state.value?.dictation_available ?? false
);
const providers = computed(() => state.value?.providers ?? []);
const cron = computed(() => state.value?.cron ?? null);
const cronJobs = computed(() => cron.value?.jobs ?? []);
const goals = computed(() => session.value?.goals ?? []);
const goalHistory = computed(() =>
  [...goals.value].sort(
    (left, right) => new Date(right.updated_at) - new Date(left.updated_at)
  )
);
const visibleGoal = computed(
  () =>
    goals.value.find(
      (goal) => goal.id === session.value?.visible_goal_id
    ) ?? null
);
const activeGoal = computed(
  () => goals.value.find((goal) => goal.status === "active") ?? null
);
const branchNodes = computed(() => session.value?.branch_nodes ?? []);
const branchLayout = computed(() => layoutBranchGraph(branchNodes.value));
const activeBranchPath = computed(
  () => new Set(session.value?.active_message_ids ?? [])
);
watch(
  () => session.value?.id,
  (current, previous) => {
    if (previous && current !== previous) {
      resetBranchNavigator();
      cancelMessageEdit();
    }
  }
);

function assistantTurnEvents(
  messages,
  activities,
  turnMessageIndex,
  sessionKey
) {
  const events = [];
  const matchedActivities = new Set();

  for (const [messageIndex, message] of messages.entries()) {
    if (message.role !== "assistant") continue;
    if (!message.hidden && message.content?.trim()) {
      events.push({
        type: "message",
        key: `message-${sessionKey}-${turnMessageIndex}-${messageIndex}`,
        message
      });
    }
    for (const toolCall of message.tool_calls ?? []) {
      const activity = activities.find(
        (candidate) =>
          candidate.turn_message_index === turnMessageIndex &&
          candidate.id === toolCall.id
      );
      if (activity) {
        events.push({
          type: "activity",
          key: `activity-${sessionKey}-${activity.id}`,
          activity
        });
        matchedActivities.add(activity.id);
      }
    }
  }

  for (const activity of activities) {
    if (
      activity.turn_message_index === turnMessageIndex &&
      !matchedActivities.has(activity.id)
    ) {
      events.push({
        type: "activity",
        key: `activity-${sessionKey}-${activity.id}`,
        activity
      });
    }
  }
  return sortChronologically(events);
}

function assistantTurnItem(key, events, turn = null) {
  const finalEventIndex = events.findLastIndex(
    (event) => event.type === "message"
  );
  const completed = turn
    ? Boolean(turn.completed_at)
    : finalEventIndex !== -1 && finalEventIndex === events.length - 1;
  const finalEvent = completed ? events[finalEventIndex] : null;
  const progressEvents = finalEvent
    ? events.filter((_, index) => index !== finalEventIndex)
    : events;
  const startedAt = Date.parse(turn?.started_at ?? "");
  const completedAt = Date.parse(turn?.completed_at ?? "");

  return {
    type: "assistant_turn",
    key,
    events,
    completed,
    finalEvent,
    progressEvents,
    operationCount: progressEvents.filter(
      (event) =>
        event.type === "activity" &&
        event.activity.tool !== "model_request"
    ).length,
    durationMs:
      Number.isFinite(startedAt) && Number.isFinite(completedAt)
        ? Math.max(0, completedAt - startedAt)
        : null
  };
}

const timeline = computed(() => {
  const messages = session.value?.messages ?? [];
  const activities = session.value?.activities ?? [];
  const turns = session.value?.turns ?? [];
  const activeMessageIds = session.value?.active_message_ids ?? [];
  const sessionKey = session.value?.id ?? "session";
  const items = [];
  let messageIndex = 0;
  while (messageIndex < messages.length) {
    const message = messages[messageIndex];
    if (message.role !== "user") {
      if (message.role === "assistant" && message.content?.trim()) {
        items.push(
          assistantTurnItem(`assistant-${sessionKey}-${messageIndex}`, [
            {
              type: "message",
              key: `message-orphan-${sessionKey}-${messageIndex}`,
              message
            }
          ])
        );
      }
      messageIndex += 1;
      continue;
    }

    if (!message.hidden && message.content?.trim()) {
      const nodeId = activeMessageIds[messageIndex] ?? null;
      items.push({
        type: "message",
        key: `message-${sessionKey}-${nodeId ?? messageIndex}`,
        message,
        nodeId
      });
    }

    let nextUser = messageIndex + 1;
    while (
      nextUser < messages.length &&
      messages[nextUser].role !== "user"
    ) {
      nextUser += 1;
    }
    const turnEvents = assistantTurnEvents(
      messages.slice(messageIndex + 1, nextUser),
      activities,
      messageIndex,
      sessionKey
    );
    if (turnEvents.length) {
      items.push(
        assistantTurnItem(
          `assistant-turn-${sessionKey}-${messageIndex}`,
          turnEvents,
          turns.find((turn) => turn.message_index === messageIndex)
        )
      );
    }
    messageIndex = nextUser;
  }
  return items;
});
const activeAssistantTurnKey = computed(() => {
  if (!sending.value) return null;
  const last = timeline.value.at(-1);
  return last?.type === "assistant_turn" ? last.key : null;
});
const virtualTimelineItems = computed(() =>
  timeline.value.slice(virtualWindow.value.start, virtualWindow.value.end)
);

watch(
  [() => session.value?.id, timeline],
  ([sessionId, items], [previousSessionId]) => {
    void activateVirtualTimeline(sessionId, items, previousSessionId);
  },
  { flush: "pre" }
);

function turnHasCollapsibleProgress(item) {
  return (
    item.completed &&
    item.finalEvent &&
    item.progressEvents.length > 0 &&
    activeAssistantTurnKey.value !== item.key
  );
}

function turnIsExpanded(item) {
  return expandedTurnKeys.value.has(item.key);
}

function visibleTurnEvents(item) {
  if (!turnHasCollapsibleProgress(item) || turnIsExpanded(item)) {
    if (item.completed && item.finalEvent) {
      return [...item.progressEvents, item.finalEvent];
    }
    return item.events;
  }
  return [item.finalEvent];
}

function toggleTurnProgress(key) {
  const next = new Set(expandedTurnKeys.value);
  if (!next.delete(key)) next.add(key);
  expandedTurnKeys.value = next;
}

function formatTurnDuration(milliseconds) {
  if (milliseconds == null) return "";
  const seconds = Math.max(1, Math.round(milliseconds / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return remainingSeconds ? `${minutes}m ${remainingSeconds}s` : `${minutes}m`;
}

function turnSummary(item) {
  const operations = `${item.operationCount} ${
    item.operationCount === 1 ? "operation" : "operations"
  }`;
  const duration = formatTurnDuration(item.durationMs);
  return duration ? `Worked for ${duration} · ${operations}` : operations;
}
const selectedModel = computed(() =>
  models.value.find((model) => model.slug === session.value?.model)
);
const reasoningOptions = computed(
  () => selectedModel.value?.supported_reasoning_levels ?? []
);
const speedOptions = computed(() =>
  (selectedModel.value?.service_tiers ?? []).filter((tier) => tier.id !== "default")
);

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...(options.headers ?? {})
    }
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(body.error || `Request failed with ${response.status}`);
  }
  return body;
}

function editProvider(provider = null) {
  providerDraft.value = provider
    ? { ...provider, api_key: "", clear_api_key: false }
    : {
        name: "",
        model: "auto",
        base_url: "https://api.openai.com/v1",
        auth: "api_key",
        api_key: "",
        clear_api_key: false
      };
}

async function saveProvider() {
  const draft = providerDraft.value;
  if (!draft?.name.trim() || !draft.base_url.trim()) return;
  await runAction(() =>
    api("/api/providers", {
      method: "POST",
      body: JSON.stringify(draft)
    })
  );
  providerDraft.value = null;
}

async function useProvider(provider) {
  await runAction(() =>
    api("/api/providers/use", {
      method: "POST",
      body: JSON.stringify({ name: provider.name })
    })
  );
}

async function deleteProvider(provider) {
  await runAction(() =>
    api("/api/providers/delete", {
      method: "POST",
      body: JSON.stringify({ name: provider.name })
    })
  );
}

async function refreshCron() {
  const snapshot = await api("/api/cron");
  if (state.value) state.value = { ...state.value, cron: snapshot };
  return snapshot;
}

async function openCron() {
  error.value = "";
  try {
    await refreshCron();
    cronOpen.value = true;
  } catch (cause) {
    error.value = cause.message;
  }
}

watch(cronOpen, (open) => {
  if (cronRefreshTimer !== null) {
    window.clearInterval(cronRefreshTimer);
    cronRefreshTimer = null;
  }
  if (open) {
    cronRefreshTimer = window.setInterval(() => {
      refreshCron().catch(() => {});
    }, 1000);
  }
});

function editCronJob(view = null) {
  cronDraftId.value = view?.id ?? "";
  cronDraftOriginalId.value = view?.id ?? "";
  cronDraft.value = view
    ? JSON.parse(JSON.stringify(view.job))
    : {
        schedule: "0 9 * * *",
        enabled: true,
        project: state.value?.project ?? "",
        prompt: "",
        provider: session.value?.provider ?? state.value?.providers?.find((item) => item.active)?.name ?? "openai",
        model: session.value?.model ?? "",
        reasoning: session.value?.reasoning_effort ?? null,
        speed: session.value?.service_tier ?? null,
        timezone: cron.value?.document?.timezone ?? "UTC",
        overlap: "skip",
        timeout_seconds: null,
        source_session_id: session.value?.id ?? null
      };
}

async function saveCronJob() {
  if (!cronDraftId.value.trim() || !cronDraft.value?.prompt?.trim()) return;
  error.value = "";
  try {
    const job = JSON.parse(JSON.stringify(cronDraft.value));
    for (const key of ["reasoning", "speed", "timezone", "source_session_id"]) {
      if (typeof job[key] === "string" && !job[key].trim()) job[key] = null;
    }
    if (!job.timeout_seconds) job.timeout_seconds = null;
    const snapshot = await api("/api/cron/jobs", {
      method: "POST",
      body: JSON.stringify({ id: cronDraftId.value.trim(), job })
    });
    state.value = { ...state.value, cron: snapshot };
    cronDraft.value = null;
    cronDraftOriginalId.value = "";
  } catch (cause) {
    error.value = cause.message;
  }
}

async function mutateCron(path, body, method = "POST") {
  error.value = "";
  try {
    const snapshot = await api(path, { method, body: JSON.stringify(body) });
    state.value = { ...state.value, cron: snapshot };
  } catch (cause) {
    error.value = cause.message;
  }
}

function toggleCronJob(view) {
  return mutateCron("/api/cron/jobs/enabled", {
    id: view.id,
    enabled: !view.job.enabled
  }, "PUT");
}

function deleteCronJob(view) {
  if (!window.confirm(`Delete scheduled task "${view.id}"?`)) return;
  return mutateCron("/api/cron/jobs/delete", { id: view.id });
}

function runCronJob(view) {
  return mutateCron("/api/cron/jobs/run", { id: view.id });
}

function installCron() {
  return mutateCron("/api/cron/install", {});
}

function uninstallCron() {
  if (!window.confirm("Uninstall the CodeCrab cron service?")) return;
  return mutateCron("/api/cron/uninstall", {});
}

function formatCronTime(value) {
  return value ? new Date(value).toLocaleString() : "—";
}

async function setCronTimezone(value) {
  const document = JSON.parse(JSON.stringify(cron.value?.document ?? {}));
  if (!value.trim() || !document.version) return;
  document.timezone = value.trim();
  error.value = "";
  try {
    const snapshot = await api("/api/cron", {
      method: "PUT",
      body: JSON.stringify(document)
    });
    state.value = { ...state.value, cron: snapshot };
  } catch (cause) {
    error.value = cause.message;
  }
}

async function loadState({ resumeGoal = false } = {}) {
  loading.value = true;
  error.value = "";
  try {
    const requestedSession = sessionIdFromLocation();
    const nextState = requestedSession
      ? await api("/api/sessions/resume", {
          method: "POST",
          body: JSON.stringify({ id: requestedSession })
        })
      : await api("/api/state");
    applyServerState(nextState, { selectActiveProject: true });
    if (!requestedSession) {
      updateSessionUrl(nextState.session?.id, "replace");
    }
    await activateComposerDraft(nextState.project, nextState.session?.id);
    if (
      resumeGoal &&
      !runtimeFor(nextState.session?.id).sending &&
      nextState.session?.goals?.some((goal) => goal.status === "active")
    ) {
      window.queueMicrotask(() => runPrompt("", { continuation: true }));
    }
  } catch (cause) {
    error.value = cause.message;
  } finally {
    loading.value = false;
    await scrollToBottom();
  }
}

async function runAction(
  action,
  {
    selectActiveProject = false,
    closeSidebar = false,
    historyMode = null
  } = {}
) {
  error.value = "";
  try {
    const nextState = await action();
    applyServerState(nextState, { selectActiveProject });
    if (historyMode) {
      updateSessionUrl(nextState.session?.id, historyMode);
    }
    if (closeSidebar) sidebarOpen.value = false;
    await scrollToBottom();
    return nextState;
  } catch (cause) {
    error.value = cause.message;
    return null;
  }
}

async function newSession(project) {
  closeAutocomplete();
  const source = await beginComposerNavigation();
  const nextState = await runAction(
    () =>
      api("/api/sessions", {
        method: "POST",
        body: JSON.stringify(newSessionPayload(project))
      }),
    {
      selectActiveProject: true,
      closeSidebar: true,
      historyMode: "push"
    }
  );
  if (nextState) {
    await activateComposerDraft(nextState.project, nextState.session?.id, {
      focusComposer: true
    });
  } else {
    await rollbackComposerNavigation(source);
  }
}

async function resumeSession(project, id) {
  closeAutocomplete();
  const source = await beginComposerNavigation();
  const nextState = await runAction(
    () =>
      api("/api/sessions/resume", {
        method: "POST",
        body: JSON.stringify({ project, id })
      }),
    {
      selectActiveProject: true,
      closeSidebar: true,
      historyMode: "push"
    }
  );
  if (!nextState) {
    await rollbackComposerNavigation(source);
    return;
  }
  await activateComposerDraft(nextState.project, nextState.session?.id);
  if (
    !runtimeFor(nextState.session?.id).sending &&
    nextState.session?.goals?.some((goal) => goal.status === "active")
  ) {
    void runPrompt("", { continuation: true });
  }
  if (runtimeFor(id).error) error.value = runtimeFor(id).error;
}

async function deleteSession(project, id) {
  if (runtimeFor(id).sending) return;
  const deletingActive =
    project === state.value?.project && id === session.value?.id;
  if (deletingActive) closeAutocomplete();
  const source = deletingActive
    ? await beginComposerNavigation()
    : null;
  error.value = "";
  try {
    const nextState = await api("/api/sessions/delete", {
      method: "POST",
      body: JSON.stringify({ project, id })
    });
    sessionStates.delete(id);
    sessionRuntimes.delete(id);
    liveObservationRevisions.delete(id);
    const nextCollapsedSessions = new Set(collapsedSessions.value);
    nextCollapsedSessions.delete(id);
    collapsedSessions.value = nextCollapsedSessions;
    forgetComposerDraft(project, id);
    virtualTimelineStore.delete(id);
    applyServerState(nextState, {
      selectActiveProject: deletingActive
    });
    if (deletingActive) {
      updateSessionUrl(nextState.session?.id, "replace");
      await activateComposerDraft(nextState.project, nextState.session?.id);
    }
  } catch (cause) {
    if (source) await rollbackComposerNavigation(source);
    error.value = cause.message;
  }
}

function preserveNewerLiveGlobals(nextState) {
  const revision = Number(nextState?.live_revision ?? 0);
  if (
    state.value &&
    !acceptsRevision(liveRevision, revision)
  ) {
    return {
      ...nextState,
      live_revision: liveRevision,
      projects: state.value.projects,
      workers: state.value.workers
    };
  }
  liveRevision = Math.max(liveRevision, revision);
  return nextState;
}

function applyServerState(nextState, { selectActiveProject = false } = {}) {
  nextState = preserveNewerLiveGlobals(nextState);
  const id = nextState.session?.id;
  const cached = id ? sessionStates.get(id) : null;
  const effective =
    id && runtimeFor(id).sending && cached
      ? { ...nextState, session: cached.session }
      : nextState;
  state.value = effective;
  if (id) sessionStates.set(id, effective);
  applyWorkerLifecycles(effective.workers ?? []);
  expandedProjects.value = expandKnownProject(
    expandedProjects.value,
    effective.projects ?? [],
    selectActiveProject ? effective.project : null
  );
}

function targetState(sessionId) {
  if (session.value?.id === sessionId) return state.value;
  return sessionStates.get(sessionId) ?? null;
}

function setTargetState(sessionId, nextState) {
  nextState = preserveNewerLiveGlobals(nextState);
  sessionStates.set(sessionId, nextState);
  if (session.value?.id === sessionId) {
    state.value = nextState;
  } else if (state.value) {
    state.value = {
      ...state.value,
      projects: nextState.projects ?? state.value.projects,
      workers: nextState.workers ?? state.value.workers,
      providers: nextState.providers ?? state.value.providers
    };
  }
}

function setLiveLifecycle(sessionId, lifecycle) {
  const runtime = runtimeFor(sessionId);
  runtime.liveLifecycle = lifecycle;
  if (runtime.localStreaming) return;
  const wasSending = runtime.sending;
  const live = liveTurnState(lifecycle);
  runtime.sending = live.sending;
  runtime.cancelling = live.cancelling;
  if (wasSending && !live.sending) {
    window.queueMicrotask(() => {
      const goalAction = runtime.pendingGoalAction;
      runtime.pendingGoalAction = null;
      if (goalAction) {
        runtime.resumeGoalAfterQueue = false;
        void goalAction().finally(() => dispatchQueuedPromptIfIdle(sessionId));
        return;
      }
      dispatchQueuedPromptIfIdle(sessionId);
    });
  }
}

function applyWorkerLifecycles(workers) {
  const lifecycleById = new Map(
    workers.map((worker) => [worker.id, worker.lifecycle])
  );
  for (const [id, runtime] of sessionRuntimes) {
    if (!runtime.localStreaming) {
      setLiveLifecycle(id, lifecycleById.get(id) ?? "idle");
    }
  }
  for (const worker of workers) {
    setLiveLifecycle(worker.id, worker.lifecycle);
  }
}

function applyLiveCatalog(message) {
  if (!state.value) return;
  const merged = mergeCatalog(state.value, message, liveRevision);
  if (!merged.applied) return;
  liveRevision = merged.revision;
  state.value = merged.state;
  applyWorkerLifecycles(message.workers ?? []);
  expandedProjects.value = expandKnownProject(
    expandedProjects.value,
    message.projects ?? [],
    null
  );
}

function liveStateFor(view) {
  const current = targetState(view.session.id);
  const shared = state.value ?? {};
  return {
    ...(current ?? shared),
    live_revision: liveRevision,
    project: view.project_root,
    session: view.session,
    projects: shared.projects ?? [],
    workers: shared.workers ?? []
  };
}

function applyLiveSession(view) {
  const id = view?.session?.id;
  if (!id) return;
  const currentRevision = liveObservationRevisions.get(id) ?? -1;
  if (view.observation_revision < currentRevision) return;
  liveObservationRevisions.set(id, view.observation_revision);
  setLiveLifecycle(id, view.lifecycle);
  if (runtimeFor(id).localStreaming) return;
  setTargetState(id, liveStateFor(view));
}

function updateLiveSessionSummary(sessionId, patch) {
  if (!state.value?.projects) return;
  state.value = {
    ...state.value,
    projects: state.value.projects.map((project) => ({
      ...project,
      sessions: project.sessions.map((item) =>
        item.id === sessionId ? { ...item, ...patch } : item
      )
    }))
  };
}

async function applySessionStreamMessage(message) {
  liveRevision = Math.max(liveRevision, Number(message.revision ?? 0));
  if (message.type === "sync" || message.type === "catalog") {
    if (message.type === "sync") sessionStreamRetryDelay = 250;
    applyLiveCatalog(message);
  }
  if (message.type === "sync") {
    for (const view of message.sessions ?? []) applyLiveSession(view);
    return;
  }
  if (message.type === "session") {
    applyLiveSession(message.session);
    return;
  }
  if (message.type !== "event") return;
  const id = message.session_id;
  const currentRevision = liveObservationRevisions.get(id) ?? -1;
  if (message.observation_revision <= currentRevision) return;
  liveObservationRevisions.set(id, message.observation_revision);
  if (runtimeFor(id).localStreaming || !targetState(id)?.session) return;
  await handleChatStreamEvent(id, message.event);
}

function scheduleSessionStreamReconnect() {
  if (sessionStreamRetry || sessionStreamController?.signal.aborted) return;
  const delay = sessionStreamRetryDelay;
  sessionStreamRetryDelay = Math.min(sessionStreamRetryDelay * 2, 5_000);
  sessionStreamRetry = window.setTimeout(() => {
    sessionStreamRetry = null;
    void connectSessionStream();
  }, delay);
}

async function connectSessionStream() {
  sessionStreamController?.abort();
  const controller = new AbortController();
  sessionStreamController = controller;
  try {
    const response = await fetch("/api/sessions/stream", {
      signal: controller.signal,
      headers: { Accept: "application/x-ndjson" }
    });
    if (!response.ok) {
      throw new Error(`Session stream failed with ${response.status}`);
    }
    await consumeNdjson(response, applySessionStreamMessage);
    if (!controller.signal.aborted) {
      throw new Error("Session stream ended unexpectedly");
    }
  } catch (cause) {
    if (cause.name !== "AbortError" && !controller.signal.aborted) {
      scheduleSessionStreamReconnect();
    }
  }
}

function projectExpanded(root) {
  return expandedProjects.value.has(root);
}

function toggleProject(root) {
  expandedProjects.value = toggleProjectExpansion(expandedProjects.value, root);
}

function sessionCollapsed(id) {
  return collapsedSessions.value.has(id);
}

function toggleSession(id) {
  collapsedSessions.value = toggleSessionExpansion(collapsedSessions.value, id);
}

function visibleProjectSessions(project) {
  return visibleSessionRows(project.sessions, collapsedSessions.value);
}

async function browseDirectory(path = directoryPathDraft.value) {
  projectPickerLoading.value = true;
  projectPickerError.value = "";
  try {
    const query = path ? `?path=${encodeURIComponent(path)}` : "";
    directoryListing.value = await api(`/api/directories${query}`);
    directoryPathDraft.value = directoryListing.value.path;
  } catch (cause) {
    projectPickerError.value = cause.message;
  } finally {
    projectPickerLoading.value = false;
  }
}

function showProjectPicker() {
  projectPickerOpen.value = true;
  directoryListing.value = null;
  directoryPathDraft.value = state.value?.project ?? "";
  newDirectoryName.value = "";
  void browseDirectory(directoryPathDraft.value);
}

async function createDirectory() {
  const name = newDirectoryName.value.trim();
  if (!name || !directoryListing.value) return;
  projectPickerLoading.value = true;
  projectPickerError.value = "";
  try {
    await api("/api/directories", {
      method: "POST",
      body: JSON.stringify({ parent: directoryListing.value.path, name })
    });
    newDirectoryName.value = "";
    await browseDirectory(directoryListing.value.path);
  } catch (cause) {
    projectPickerError.value = cause.message;
  } finally {
    projectPickerLoading.value = false;
  }
}

async function openProject(path = directoryListing.value?.path) {
  if (!path) return;
  const source = await beginComposerNavigation();
  const nextState = await runAction(
    () =>
      api("/api/projects/open", {
        method: "POST",
        body: JSON.stringify({ path })
      }),
    { selectActiveProject: true, historyMode: "replace" }
  );
  if (!nextState) {
    await rollbackComposerNavigation(source);
    return;
  }
  projectPickerOpen.value = false;
  await activateComposerDraft(nextState.project, nextState.session?.id);
}

function sessionIdFromLocation() {
  const match = window.location.pathname.match(/^\/sessions\/([^/]+)\/?$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function updateSessionUrl(id, mode = "push") {
  const url = id ? `/sessions/${encodeURIComponent(id)}` : "/";
  if (window.location.pathname === url) return;
  if (mode === "replace") window.history.replaceState(null, "", url);
  else window.history.pushState(null, "", url);
}

async function handleHistoryNavigation() {
  const id = sessionIdFromLocation();
  if (!id || id === session.value?.id) return;
  closeAutocomplete();
  const source = await beginComposerNavigation();
  error.value = "";
  try {
    const nextState = await api("/api/sessions/resume", {
      method: "POST",
      body: JSON.stringify({ id })
    });
    applyServerState(nextState, { selectActiveProject: true });
    await activateComposerDraft(nextState.project, nextState.session?.id);
    await scrollToBottom();
    if (!sending.value && activeGoal.value) {
      void runPrompt("", { continuation: true });
    }
  } catch (cause) {
    await rollbackComposerNavigation(source);
    updateSessionUrl(source.identity?.sessionId, "replace");
    error.value = cause.message;
  }
}

function projectName(root) {
  const trimmed = root?.replace(/[\\/]+$/, "") ?? "";
  return trimmed.split(/[\\/]/).at(-1) || trimmed;
}

function isCurrentSession(project, item) {
  return project.root === state.value?.project && item.id === session.value?.id;
}

function workerLifecycle(id) {
  if (runtimeFor(id).error) return "failed";
  if (runtimeFor(id).sending) return "running";
  return state.value?.workers?.find((worker) => worker.id === id)?.lifecycle ?? "";
}

function resetBranchNavigator() {
  branchesOpen.value = false;
  selectedBranchNode.value = null;
  hoveredBranchNode.value = null;
  branchHoverRequest += 1;
  branchSelecting.value = false;
  branchOriginalPath.value = new Set();
  branchOriginalState = null;
}

function openBranchNavigator() {
  if (editingMessageNode.value) {
    error.value = "Save or cancel the message edit before browsing branches.";
    return;
  }
  if (sending.value) {
    error.value = "Wait for the active operation before browsing branches.";
    return;
  }
  if (!branchNodes.value.length) {
    error.value = "This conversation has no visible user messages.";
    return;
  }
  branchOriginalState = state.value;
  branchOriginalPath.value = new Set(
    session.value?.active_message_ids ?? []
  );
  branchOriginalScrollTop = conversation.value?.scrollTop ?? 0;
  branchOriginalAutoScroll = autoScroll.value;
  selectedBranchNode.value = latestActiveBranchNodeId(
    branchNodes.value,
    session.value?.active_message_ids
  );
  branchesOpen.value = true;
  closeAutocomplete();
  error.value = "";
}

async function cancelBranchNavigator() {
  if (branchSelecting.value || sending.value) return;
  const original = branchOriginalState;
  const scrollTop = branchOriginalScrollTop;
  const followBottom = branchOriginalAutoScroll;
  resetBranchNavigator();
  if (!original) return;
  applyServerState(original);
  await nextTick();
  await nextTick();
  autoScroll.value = followBottom;
  if (conversation.value) conversation.value.scrollTop = scrollTop;
  updateVirtualWindow();
}

async function previewBranch(nodeId) {
  if (
    branchSelecting.value ||
    editingMessageNode.value ||
    sending.value ||
    !session.value?.id
  ) {
    return;
  }
  branchSelecting.value = true;
  error.value = "";
  try {
    const nextState = await api("/api/branches/preview", {
      method: "POST",
      body: JSON.stringify({
        session_id: session.value.id,
        node_id: nodeId
      })
    });
    applyServerState(nextState);
    selectedBranchNode.value = nodeId;
    hoveredBranchNode.value = nodeId;
    branchHoverRequest += 1;
    await scrollToBranchMessage(nodeId);
  } catch (cause) {
    error.value = cause.message;
  } finally {
    branchSelecting.value = false;
  }
}

async function confirmBranchSelection() {
  const nodeId = selectedBranchNode.value;
  if (
    !nodeId ||
    branchSelecting.value ||
    editingMessageNode.value ||
    sending.value ||
    !session.value?.id
  ) {
    return;
  }
  branchSelecting.value = true;
  error.value = "";
  try {
    const nextState = await api("/api/branches/select", {
      method: "POST",
      body: JSON.stringify({
        session_id: session.value.id,
        node_id: nodeId
      })
    });
    applyServerState(nextState);
    resetBranchNavigator();
    await scrollToBranchMessage(nodeId);
  } catch (cause) {
    error.value = cause.message;
    branchSelecting.value = false;
  }
}

function branchEdgeClasses(edge) {
  return {
    "branch-edge-current": routeContainsEdge(activeBranchPath.value, edge),
    "branch-edge-original":
      !routeContainsEdge(activeBranchPath.value, edge) &&
      routeContainsEdge(branchOriginalPath.value, edge)
  };
}

function branchNodeClasses(node) {
  return {
    "branch-node-current": activeBranchPath.value.has(node.id),
    "branch-node-original":
      !activeBranchPath.value.has(node.id) &&
      branchOriginalPath.value.has(node.id),
    "branch-node-selected": selectedBranchNode.value === node.id
  };
}

async function hoverBranchNode(nodeId) {
  hoveredBranchNode.value = nodeId;
  const request = ++branchHoverRequest;
  await scrollToBranchMessage(nodeId, {
    revealOnly: true,
    current: () =>
      request === branchHoverRequest &&
      hoveredBranchNode.value === nodeId
  });
}

function leaveBranchNode(nodeId) {
  if (hoveredBranchNode.value !== nodeId) return;
  hoveredBranchNode.value = null;
  branchHoverRequest += 1;
}

function rebaseBranchNavigator() {
  if (!branchesOpen.value) return;
  branchOriginalState = state.value;
  branchOriginalPath.value = new Set(
    session.value?.active_message_ids ?? []
  );
  branchOriginalScrollTop = conversation.value?.scrollTop ?? 0;
  branchOriginalAutoScroll = autoScroll.value;
  selectedBranchNode.value = latestActiveBranchNodeId(
    branchNodes.value,
    session.value?.active_message_ids
  );
}

async function beginMessageEdit(item) {
  if (
    sending.value ||
    branchSelecting.value ||
    !item.nodeId ||
    editingMessageNode.value ||
    recalledMessageNode.value
  ) {
    return;
  }
  editingMessageNode.value = item.nodeId;
  messageEditDraft.value = item.message.content ?? "";
  messageEditAttachments.value = bindingsFromMessage(item.message);
  closeAutocomplete();
  error.value = "";
  await nextTick();
  messageEditor.value?.focus();
  messageEditor.value?.setSelectionRange(
    messageEditDraft.value.length,
    messageEditDraft.value.length
  );
}

function cancelMessageEdit() {
  editingMessageNode.value = null;
  messageEditDraft.value = "";
  messageEditAttachments.value = [];
}

async function recallLatestUserMessage() {
  if (
    sending.value ||
    recording.value ||
    transcribing.value ||
    branchesOpen.value ||
    editingMessageNode.value
  ) {
    return false;
  }
  const recalled = recalledMessageForComposer(
    session.value,
    draft.value,
    recalledMessageNode.value
  );
  if (!recalled) return false;
  recalledMessageNode.value = recalled.nodeId;
  draft.value = recalled.content;
  const recalledIndex = session.value.active_message_ids.indexOf(recalled.nodeId);
  draftAttachments.value = bindingsFromMessage(
    session.value.messages[recalledIndex]
  );
  closeAutocomplete();
  error.value = "";
  await nextTick();
  resizeComposer();
  composer.value?.focus();
  composer.value?.setSelectionRange(draft.value.length, draft.value.length);
  return true;
}

async function cancelRecalledMessageEdit() {
  if (!recalledMessageNode.value) return false;
  recalledMessageNode.value = null;
  await clearComposer();
  closeAutocomplete();
  error.value = "";
  return true;
}

function projectEditedMessage(nodeId, content) {
  const sessionId = session.value?.id;
  const current = targetState(sessionId)?.session;
  const projected = projectEditedSession(
    current,
    nodeId,
    content,
    new Date().toISOString()
  );
  if (!projected) return false;
  updateTargetSession(sessionId, () => projected);
  return true;
}

async function saveMessageEdit() {
  const nodeId = editingMessageNode.value;
  const { prompt, attachments } = trimAttachmentDraft(
    messageEditDraft.value,
    messageEditAttachments.value
  );
  if (!nodeId || !prompt || sending.value) return;
  if (!projectEditedMessage(nodeId, prompt)) {
    error.value = "The message is no longer on the visible branch.";
    return;
  }
  const keepBranchesOpen = branchesOpen.value;
  cancelMessageEdit();
  await runPrompt(prompt, {
    editNodeId: nodeId,
    attachmentBindings: attachments
  });
  if (keepBranchesOpen && branchesOpen.value) {
    await nextTick();
    rebaseBranchNavigator();
  }
}

function handleMessageEditorKey(event) {
  if (event.key === "Escape") {
    event.preventDefault();
    cancelMessageEdit();
  } else if (
    event.key === "Enter" &&
    !event.shiftKey &&
    !event.altKey
  ) {
    event.preventDefault();
    void saveMessageEdit();
  }
}

function applyGoalState(nextState) {
  applyServerState(nextState);
}

async function goalRequest(
  path,
  { method = "POST", sessionId = session.value?.id, id, objective } = {}
) {
  const nextState = await api(`/api/goals/${path}`, {
    method,
    body: JSON.stringify({ session_id: sessionId, id, objective })
  });
  if (session.value?.id === sessionId) applyGoalState(nextState);
  else setTargetState(sessionId, nextState);
  return nextState;
}

async function afterCurrentTurn(action) {
  const sessionId = session.value?.id;
  const guarded = async () => {
    error.value = "";
    try {
      await action(sessionId);
    } catch (cause) {
      error.value = cause.message;
    }
  };
  if (sending.value) {
    selectedRuntime.value.pendingGoalAction = guarded;
    await cancelTurn({ pauseGoal: false });
    return;
  }
  await guarded();
}

async function createGoal(objective) {
  await afterCurrentTurn(async (sessionId) => {
    await goalRequest("create", { sessionId, objective });
    void runPrompt(objective, { sessionId });
  });
}

async function toggleGoal(goal) {
  await afterCurrentTurn(async (sessionId) => {
    if (goal.status === "active") {
      await goalRequest("pause", { sessionId, id: goal.id });
    } else {
      await goalRequest("activate", { sessionId, id: goal.id });
      goalsOpen.value = false;
      void runPrompt("", { continuation: true, sessionId });
    }
  });
}

async function deleteGoal(goal) {
  await afterCurrentTurn(async (sessionId) => {
    await goalRequest("delete", { sessionId, id: goal.id });
    if (describedGoal.value?.id === goal.id) describedGoal.value = null;
    if (editingGoal.value?.id === goal.id) editingGoal.value = null;
  });
}

async function beginGoalEdit(goal) {
  const resume = goal.status === "active";
  await afterCurrentTurn(async (sessionId) => {
    if (resume) await goalRequest("pause", { sessionId, id: goal.id });
    editingGoal.value = { ...goal, sessionId };
    goalDraft.value = goal.objective;
    editingGoalResume.value = resume;
  });
}

async function saveGoalEdit() {
  const goal = editingGoal.value;
  const objective = goalDraft.value.trim();
  if (!goal || !objective) return;
  try {
    const sessionId = goal.sessionId ?? session.value?.id;
    await goalRequest("edit", {
      method: "PUT",
      sessionId,
      id: goal.id,
      objective
    });
    const resume = editingGoalResume.value;
    editingGoal.value = null;
    editingGoalResume.value = false;
    if (resume) {
      await goalRequest("activate", { sessionId, id: goal.id });
      void runPrompt("", { continuation: true, sessionId });
    }
  } catch (cause) {
    error.value = cause.message;
  }
}

async function cancelGoalEdit() {
  const goal = editingGoal.value;
  const resume = editingGoalResume.value;
  editingGoal.value = null;
  editingGoalResume.value = false;
  if (goal && resume) {
    try {
      const sessionId = goal.sessionId ?? session.value?.id;
      await goalRequest("activate", { sessionId, id: goal.id });
      void runPrompt("", { continuation: true, sessionId });
    } catch (cause) {
      error.value = cause.message;
    }
  }
}

function goalStatusLabel(status) {
  return {
    active: "Active",
    paused: "Paused",
    completed: "Done",
    blocked: "Blocked"
  }[status] ?? status;
}

function compactGoal(goal, length = 96) {
  const text = goal.objective.replace(/\s+/g, " ").trim();
  return text.length > length ? `${text.slice(0, length - 1)}…` : text;
}

function stopMicrophoneTracks() {
  stopWaveform();
  microphoneStream?.getTracks().forEach((track) => track.stop());
  microphoneStream = null;
  recording.value = false;
}

function stopWaveform() {
  if (waveformFrame !== null) {
    window.cancelAnimationFrame(waveformFrame);
    waveformFrame = null;
  }
  microphoneSource?.disconnect();
  audioAnalyser?.disconnect();
  microphoneSource = null;
  audioAnalyser = null;
  waveformData = null;
  if (audioContext) {
    void audioContext.close();
    audioContext = null;
  }
}

function drawWaveform() {
  if (!audioAnalyser || !waveformData || !recording.value) return;
  const canvas = waveformCanvas.value;
  if (!canvas) {
    waveformFrame = window.requestAnimationFrame(drawWaveform);
    return;
  }
  const bounds = canvas.getBoundingClientRect();
  const scale = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(bounds.width * scale));
  const height = Math.max(1, Math.round(bounds.height * scale));
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }

  audioAnalyser.getByteTimeDomainData(waveformData);
  const context = canvas.getContext("2d");
  if (!context) return;
  context.clearRect(0, 0, width, height);
  context.lineWidth = Math.max(1, 1.25 * scale);
  context.strokeStyle = "rgb(248 113 113 / 0.9)";
  context.beginPath();
  const step = width / Math.max(1, waveformData.length - 1);
  for (let index = 0; index < waveformData.length; index += 1) {
    const value = (waveformData[index] - 128) / 128;
    const x = index * step;
    const y = height / 2 + value * height * 0.42;
    if (index === 0) context.moveTo(x, y);
    else context.lineTo(x, y);
  }
  context.stroke();
  waveformFrame = window.requestAnimationFrame(drawWaveform);
}

async function startWaveform(stream) {
  stopWaveform();
  const AudioContext = window.AudioContext || window.webkitAudioContext;
  if (!AudioContext) return;
  audioContext = new AudioContext();
  if (audioContext.state === "suspended") await audioContext.resume();
  if (!recording.value || microphoneStream !== stream) {
    stopWaveform();
    return;
  }
  microphoneSource = audioContext.createMediaStreamSource(stream);
  audioAnalyser = audioContext.createAnalyser();
  audioAnalyser.fftSize = 1024;
  waveformData = new Uint8Array(audioAnalyser.fftSize);
  microphoneSource.connect(audioAnalyser);
  await nextTick();
  waveformFrame = window.requestAnimationFrame(drawWaveform);
}

async function insertTranscript(text) {
  const element = composer.value;
  const focused = document.activeElement === element;
  const result = insertTranscriptAtSelection(draft.value, text, {
    focused,
    start: focused ? element.selectionStart : draft.value.length,
    end: focused ? element.selectionEnd : draft.value.length
  });
  if (!result.inserted) return false;
  draft.value = result.value;
  await nextTick();
  element?.focus();
  element?.setSelectionRange(result.cursor, result.cursor);
  resizeComposer();
  await refreshAutocomplete(element);
  return true;
}

async function transcribeRecording() {
  recordingStopping = false;
  const discard = discardRecording;
  discardRecording = false;
  const shouldSend = sendAfterTranscription;
  sendAfterTranscription = false;
  const contentType =
    mediaRecorder?.mimeType || recordingChunks[0]?.type || "audio/webm";
  const blob = new Blob(recordingChunks, { type: contentType });
  recordingChunks = [];
  mediaRecorder = null;
  stopMicrophoneTracks();
  if (discard) return;
  if (!blob.size) {
    error.value = "The microphone did not capture any audio.";
    return;
  }

  transcribing.value = true;
  error.value = "";
  let transcriptInserted = false;
  try {
    const response = await fetch("/api/transcribe", {
      method: "POST",
      headers: {
        "Content-Type": contentType,
        "X-CodeCrab-Session": session.value?.id ?? ""
      },
      body: blob
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(body.error || `Request failed with ${response.status}`);
    }
    transcriptInserted = await insertTranscript(body.text);
  } catch (cause) {
    error.value = `Dictation failed: ${cause.message}`;
  } finally {
    transcribing.value = false;
  }
  if (shouldSend && transcriptInserted) {
    await sendPrompt();
  }
}

function cancelDictation() {
  if (!recording.value) return false;
  discardRecording = true;
  sendAfterTranscription = false;
  error.value = "";
  if (!recordingStopping && mediaRecorder?.state === "recording") {
    recordingStopping = true;
    mediaRecorder.stop();
  }
  recordingChunks = [];
  stopMicrophoneTracks();
  return true;
}

async function toggleDictation() {
  if (recording.value) {
    if (recordingStopping) return;
    recordingStopping = true;
    sendAfterTranscription = false;
    mediaRecorder?.stop();
    return;
  }
  if (recordingStopping || transcribing.value || !dictationAvailable.value) return;
  error.value = "";
  try {
    discardRecording = false;
    sendAfterTranscription = false;
    microphoneStream = await navigator.mediaDevices.getUserMedia({
      audio: { channelCount: 1 }
    });
    recordingChunks = [];
    const preferredType = "audio/webm;codecs=opus";
    const options =
      window.MediaRecorder?.isTypeSupported?.(preferredType)
        ? { mimeType: preferredType }
        : undefined;
    mediaRecorder = new MediaRecorder(microphoneStream, options);
    mediaRecorder.addEventListener("dataavailable", (event) => {
      if (event.data.size) recordingChunks.push(event.data);
    });
    mediaRecorder.addEventListener("stop", transcribeRecording, {
      once: true
    });
    mediaRecorder.addEventListener(
      "error",
      (event) => {
        discardRecording = true;
        recordingStopping = false;
        error.value = `Dictation failed: ${event.error?.message || "microphone recording failed"}`;
        stopMicrophoneTracks();
      },
      { once: true }
    );
    mediaRecorder.start();
    recording.value = true;
    try {
      await startWaveform(microphoneStream);
    } catch {
      stopWaveform();
    }
  } catch (cause) {
    stopMicrophoneTracks();
    error.value = `Dictation failed: ${cause.message}`;
  }
}

async function updateModel(patch) {
  if (!session.value || branchesOpen.value || editingMessageNode.value) return;
  const sessionId = session.value.id;
  const currentSession = session.value;
  error.value = "";
  try {
    const nextState = await api("/api/model", {
      method: "PUT",
      body: JSON.stringify({
        session_id: sessionId,
        model: patch.model ?? currentSession.model,
        reasoning_effort:
          patch.reasoning_effort === undefined
            ? currentSession.reasoning_effort
            : patch.reasoning_effort,
        service_tier:
          patch.service_tier === undefined
            ? currentSession.service_tier
            : patch.service_tier
      })
    });
    if (session.value?.id === sessionId) applyServerState(nextState);
    else setTargetState(sessionId, nextState);
  } catch (cause) {
    error.value = cause.message;
  }
}

async function chooseModel(event) {
  const model = models.value.find((item) => item.slug === event.target.value);
  await updateModel({
    model: event.target.value,
    reasoning_effort: model?.default_reasoning_level ?? null,
    service_tier:
      model?.default_service_tier === "default"
        ? null
        : model?.default_service_tier ?? null
  });
}

function updateTargetSession(sessionId, update) {
  const currentState = targetState(sessionId);
  if (!currentState?.session) return;
  setTargetState(sessionId, {
    ...currentState,
    session: update(currentState.session)
  });
}

function applyActivity(sessionId, activity) {
  const target = targetState(sessionId)?.session;
  const current = target?.activities ?? [];
  const index = current.findIndex((item) => item.id === activity.id);
  const activities =
    index === -1
      ? [...current, activity]
      : current.map((item, itemIndex) =>
          itemIndex === index ? activity : item
        );
  updateTargetSession(sessionId, (targetSession) => ({
      ...targetSession,
      activities
  }));
}

function applyAssistantMessage(sessionId, message) {
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    messages: [...(targetSession.messages ?? []), message]
  }));
}

function applyUserMessage(sessionId, message) {
  const current = targetState(sessionId)?.session;
  const firstVisibleMessage = !(current?.messages ?? []).some(
    (item) => !item.hidden && (item.role === "user" || item.role === "assistant")
  );
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    title:
      firstVisibleMessage && message.content
        ? message.content.slice(0, 72)
        : targetSession.title,
    updated_at: message.created_at ?? targetSession.updated_at,
    messages: [...(targetSession.messages ?? []), message]
  }));
  if (firstVisibleMessage && message.content) {
    updateLiveSessionSummary(sessionId, {
      title: message.content.slice(0, 72),
      updated_at: message.created_at
    });
  }
}

function applyEditedUserMessage(sessionId, message) {
  const messages = [...(targetState(sessionId)?.session?.messages ?? [])];
  if (messages.at(-1)?.role === "user") {
    messages[messages.length - 1] = message;
  } else {
    messages.push(message);
  }
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    messages
  }));
}

function applyAssistantTextDelta(sessionId, delta, start, sequence, createdAt) {
  const messages = [...(targetState(sessionId)?.session?.messages ?? [])];
  if (start || messages.at(-1)?.role !== "assistant") {
    messages.push({
      role: "assistant",
      content: delta,
      sequence,
      created_at: createdAt
    });
  } else {
    const last = messages.at(-1);
    messages[messages.length - 1] = {
      ...last,
      sequence,
      content: (last.content ?? "") + delta
    };
  }
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    messages
  }));
}

function applyAssistantStreamReset(sessionId) {
  const messages = [...(targetState(sessionId)?.session?.messages ?? [])];
  if (messages.at(-1)?.role === "assistant") {
    messages.pop();
  }
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    messages
  }));
}

function applyAssistantMessageCompleted(sessionId, message) {
  const messages = [...(targetState(sessionId)?.session?.messages ?? [])];
  const index = messages.findLastIndex((item) => item.role === "assistant");
  if (index === -1) messages.push(message);
  else messages[index] = message;
  updateTargetSession(sessionId, (targetSession) => ({
    ...targetSession,
    messages
  }));
}

async function handleChatStreamEvent(
  sessionId,
  event,
  { editing = false } = {}
) {
  if (event.type === "user_message") {
    if (editing) applyEditedUserMessage(sessionId, event.message);
    else applyUserMessage(sessionId, event.message);
    if (session.value?.id === sessionId) await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_message") {
    applyAssistantMessage(sessionId, event.message);
    if (session.value?.id === sessionId) await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_text_delta") {
    applyAssistantTextDelta(
      sessionId,
      event.delta,
      event.start,
      event.sequence,
      event.created_at
    );
    if (session.value?.id === sessionId) await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_stream_reset") {
    applyAssistantStreamReset(sessionId);
    return false;
  }
  if (event.type === "assistant_message_completed") {
    applyAssistantMessageCompleted(sessionId, event.message);
    return false;
  }
  if (event.type === "activity") {
    applyActivity(sessionId, event.activity);
    if (session.value?.id === sessionId) await scrollToBottom();
    return false;
  }
  if (event.type === "done") {
    setTargetState(sessionId, event.state);
    applyWorkerLifecycles(event.state.workers ?? []);
    return true;
  }
  if (event.type === "cancelled") {
    setTargetState(sessionId, event.state);
    applyWorkerLifecycles(event.state.workers ?? []);
    return true;
  }
  if (event.type === "error") {
    throw new Error(event.error || "The agent turn failed");
  }
  return false;
}

async function streamChat(
  prompt,
  {
    continuation = false,
    sessionId = session.value?.id,
    editNodeId = null,
    attachments = []
  } = {}
) {
  if (!sessionId) throw new Error("Create or resume a session first");
  const response = await fetch("/api/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      session_id: sessionId,
      prompt,
      continuation,
      edit_node_id: editNodeId,
      attachments
    })
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error || `Request failed with ${response.status}`);
  }
  if (!response.body) {
    throw new Error("The browser did not expose the response stream");
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let completed = false;
  while (true) {
    const { value, done } = await reader.read();
    buffer += decoder.decode(value ?? new Uint8Array(), { stream: !done });
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (line.trim()) {
        completed =
          (await handleChatStreamEvent(sessionId, JSON.parse(line), {
            editing: Boolean(editNodeId)
          })) || completed;
      }
    }
    if (done) break;
  }
  if (buffer.trim()) {
    completed =
      (await handleChatStreamEvent(sessionId, JSON.parse(buffer), {
        editing: Boolean(editNodeId)
      })) || completed;
  }
  if (!completed) {
    throw new Error("The agent stream ended before returning its final state");
  }
}

async function sendPrompt() {
  if (editingMessageNode.value) {
    error.value = "Save or cancel the message edit first.";
    return;
  }
  if (branchesOpen.value) {
    error.value = "Keep or cancel the branch preview before writing.";
    return;
  }
  if (recording.value) {
    if (recordingStopping) return;
    recordingStopping = true;
    sendAfterTranscription = true;
    mediaRecorder?.stop();
    return;
  }
  if (transcribing.value) return;
  const { prompt, attachments } = trimAttachmentDraft(
    draft.value,
    draftAttachments.value
  );
  const runtime = selectedRuntime.value;
  if (!prompt) {
    if (runtime.queuedPromptEdit) {
      error.value = "A queued message cannot be empty; delete it instead.";
    }
    return;
  }
  if (runtime.queuedPromptEdit) {
    await finishQueuedPromptEdit(prompt, attachments);
    return;
  }
  if (recalledMessageNode.value) {
    const nodeId = recalledMessageNode.value;
    if (!projectEditedMessage(nodeId, prompt)) {
      error.value = "The recalled message is no longer on the visible branch.";
      return;
    }
    const sent = composerDrafts.snapshot();
    const sentAttachments = draftAttachments.value.map((binding) => ({ ...binding }));
    recalledMessageNode.value = null;
    await clearComposer({ keepStored: true });
    const completed = await runPrompt(prompt, {
      editNodeId: nodeId,
      attachmentBindings: attachments
    });
    await composerDrafts.finishSend(sent, completed);
    finishAttachmentSend(sent, completed);
    if (!completed && !draftAttachments.value.length) {
      draftAttachments.value = sentAttachments;
    }
    if (
      !completed &&
      session.value?.active_message_ids?.includes(nodeId)
    ) {
      recalledMessageNode.value = nodeId;
    }
    return;
  }
  if (prompt === "/goals") {
    await clearComposer();
    closeAutocomplete();
    goalsOpen.value = true;
    return;
  }
  if (prompt === "/cron") {
    await clearComposer();
    closeAutocomplete();
    await openCron();
    return;
  }
  if (prompt === "/branches") {
    await clearComposer();
    openBranchNavigator();
    return;
  }
  if (prompt === "/goal") {
    error.value = "Write the objective after /goal.";
    return;
  }
  const goalMatch = prompt.match(/^\/goal\s+([\s\S]+)$/);
  if (goalMatch) {
    await clearComposer();
    closeAutocomplete();
    await createGoal(goalMatch[1].trim());
    return;
  }
  if (sending.value) {
    enqueuePrompt(
      runtime.promptQueue,
      prompt,
      attachments,
      draftAttachments.value.map((binding) => ({ ...binding }))
    );
    await clearComposer();
    closeAutocomplete();
    return;
  }
  const sent = composerDrafts.snapshot();
  const sentAttachments = draftAttachments.value.map((binding) => ({ ...binding }));
  await clearComposer({ keepStored: true });
  const completed = await runPrompt(prompt, { attachmentBindings: attachments });
  await composerDrafts.finishSend(sent, completed);
  finishAttachmentSend(sent, completed);
  if (!completed && !draftAttachments.value.length) {
    draftAttachments.value = sentAttachments;
  }
}

function activeGoalFor(sessionId) {
  return (
    targetState(sessionId)?.session?.goals?.find(
      (goal) => goal.status === "active"
    ) ?? null
  );
}

async function runPrompt(
  prompt,
  {
    continuation = false,
    sessionId = session.value?.id,
    editNodeId = null,
    attachmentBindings = []
  } = {}
) {
  if (!sessionId) return;
  const runtime = runtimeFor(sessionId);
  if (session.value?.id === sessionId) closeAutocomplete();
  runtime.localStreaming = true;
  runtime.liveLifecycle = "running";
  runtime.sending = true;
  runtime.cancelling = false;
  if (session.value?.id === sessionId) error.value = "";

  if (session.value?.id === sessionId) await scrollToBottom();

  let completed = false;
  try {
    await streamChat(prompt, {
      continuation,
      sessionId,
      editNodeId,
      attachments: attachmentBindings
    });
    completed = true;
    runtime.error = "";
  } catch (cause) {
    const streamError = cause.message;
    runtime.error = streamError;
    try {
      const latest = await api(
        `/api/state?session_id=${encodeURIComponent(sessionId)}`
      );
      if (session.value?.id === sessionId) applyServerState(latest);
      else setTargetState(sessionId, latest);
      applyWorkerLifecycles(latest.workers ?? []);
    } catch {
      // The live session feed may still recover the authoritative lifecycle.
    }
    if (session.value?.id === sessionId) error.value = streamError;
  } finally {
    runtime.localStreaming = false;
    const streamEnd = localStreamEndState(completed, runtime.liveLifecycle);
    if (streamEnd.keepFollowing) {
      runtime.sending = streamEnd.sending;
      runtime.cancelling = streamEnd.cancelling;
      if (session.value?.id === sessionId) await scrollToBottom();
      return completed;
    }
    runtime.sending = false;
    runtime.cancelling = false;
    if (session.value?.id === sessionId) await scrollToBottom();
    const goalAction = runtime.pendingGoalAction;
    runtime.pendingGoalAction = null;
    if (goalAction) {
      runtime.resumeGoalAfterQueue = false;
      void goalAction().finally(() => dispatchQueuedPromptIfIdle(sessionId));
      return;
    }
    runtime.resumeGoalAfterQueue =
      completed && Boolean(activeGoalFor(sessionId));
    const nextPrompt = takeNextQueuedPrompt(runtime.promptQueue);
    if (nextPrompt) {
      void runPrompt(nextPrompt.content, {
        sessionId,
        attachmentBindings: nextPrompt.attachments ?? []
      });
    } else if (
      runtime.promptQueue.items.length === 0 &&
      runtime.resumeGoalAfterQueue
    ) {
      runtime.resumeGoalAfterQueue = false;
      void runPrompt("", { continuation: true, sessionId });
    } else if (session.value?.id === sessionId) {
      composer.value?.focus();
    }
  }
  return completed;
}

async function cancelTurn({ pauseGoal = true } = {}) {
  const sessionId = session.value?.id;
  const runtime = runtimeFor(sessionId);
  if (!sessionId || !runtime.sending || runtime.cancelling) return;
  const goal = activeGoalFor(sessionId);
  if (pauseGoal && goal && !runtime.pendingGoalAction) {
    const id = goal.id;
    runtime.pendingGoalAction = () =>
      goalRequest("pause", { sessionId, id });
  }
  runtime.cancelling = true;
  error.value = "";
  try {
    await api("/api/chat/cancel", {
      method: "POST",
      body: JSON.stringify({ session_id: sessionId })
    });
  } catch (cause) {
    runtime.cancelling = false;
    error.value = `Could not stop the agent: ${cause.message}`;
  }
}

async function beginQueuedPromptEdit(item) {
  const runtime = selectedRuntime.value;
  if (runtime.queuedPromptEdit?.id === item.id) {
    composer.value?.focus();
    return;
  }
  if (runtime.queuedPromptEdit) {
    await cancelQueuedPromptEdit({ dispatch: false });
  }
  runtime.queuedPromptEdit = {
    id: item.id,
    previousDraft: draft.value,
    previousAttachments: draftAttachments.value.map((binding) => ({ ...binding }))
  };
  runtime.promptQueue.editingId = item.id;
  draft.value = item.content;
  draftAttachments.value = (item.composerAttachments ?? []).map((binding) => ({
    ...binding
  }));
  closeAutocomplete();
  error.value = "";
  await nextTick();
  resizeComposer();
  composer.value?.focus();
  composer.value?.setSelectionRange(draft.value.length, draft.value.length);
}

async function restoreDraftAfterQueuedPromptEdit(runtime, edit) {
  runtime.queuedPromptEdit = null;
  runtime.promptQueue.editingId = null;
  draft.value = edit.previousDraft;
  draftAttachments.value = edit.previousAttachments;
  closeAutocomplete();
  await nextTick();
  resizeComposer();
  composer.value?.focus();
}

function dispatchQueuedPromptIfIdle(sessionId) {
  const runtime = runtimeFor(sessionId);
  if (runtime.sending || runtime.pendingGoalAction) return false;
  const nextPrompt = takeNextQueuedPrompt(runtime.promptQueue);
  if (nextPrompt) {
    void runPrompt(nextPrompt.content, {
      sessionId,
      attachmentBindings: nextPrompt.attachments ?? []
    });
    return true;
  }
  if (
    runtime.promptQueue.items.length === 0 &&
    runtime.resumeGoalAfterQueue &&
    activeGoalFor(sessionId)
  ) {
    runtime.resumeGoalAfterQueue = false;
    void runPrompt("", { continuation: true, sessionId });
    return true;
  }
  return false;
}

async function finishQueuedPromptEdit(content, attachments) {
  const runtime = selectedRuntime.value;
  const edit = runtime.queuedPromptEdit;
  if (!edit) return;
  if (
    !updateQueuedPrompt(
      runtime.promptQueue,
      edit.id,
      content,
      attachments,
      draftAttachments.value.map((binding) => ({ ...binding }))
    )
  ) {
    error.value = "The queued message no longer exists.";
  } else {
    error.value = "";
  }
  const sessionId = session.value?.id;
  await restoreDraftAfterQueuedPromptEdit(runtime, edit);
  dispatchQueuedPromptIfIdle(sessionId);
}

async function cancelQueuedPromptEdit({ dispatch = true } = {}) {
  const runtime = selectedRuntime.value;
  const edit = runtime.queuedPromptEdit;
  if (!edit) return;
  const sessionId = session.value?.id;
  await restoreDraftAfterQueuedPromptEdit(runtime, edit);
  if (dispatch) dispatchQueuedPromptIfIdle(sessionId);
}

async function deleteQueuedPrompt(item) {
  const runtime = selectedRuntime.value;
  const edit =
    runtime.queuedPromptEdit?.id === item.id
      ? runtime.queuedPromptEdit
      : null;
  if (edit) {
    await restoreDraftAfterQueuedPromptEdit(runtime, edit);
  }
  removeQueuedPrompt(runtime.promptQueue, item.id);
  dispatchQueuedPromptIfIdle(session.value?.id);
}

function steerQueuedPrompt(item) {
  const runtime = selectedRuntime.value;
  if (runtime.queuedPromptEdit?.id === item.id) return;
  if (!prioritizeQueuedPrompt(runtime.promptQueue, item.id)) return;
  if (runtime.sending) {
    cancelTurn({ pauseGoal: false });
  } else {
    dispatchQueuedPromptIfIdle(session.value?.id);
  }
}

function handleGlobalKeydown(event) {
  if (event.repeat) return;
  const platform =
    navigator.userAgentData?.platform ?? navigator.platform ?? navigator.userAgent;
  if (
    isDictationShortcut(event, platform) &&
    !branchesOpen.value &&
    !editingMessageNode.value &&
    !recordingStopping &&
    (recording.value || (!transcribing.value && dictationAvailable.value))
  ) {
    event.preventDefault();
    lastEscapeAt = 0;
    void toggleDictation();
    return;
  }
  if (event.key !== "Escape") {
    lastEscapeAt = 0;
    return;
  }
  if (recording.value) {
    event.preventDefault();
    lastEscapeAt = 0;
    cancelDictation();
    return;
  }
  if (editingMessageNode.value) {
    event.preventDefault();
    cancelMessageEdit();
    return;
  }
  if (recalledMessageNode.value) {
    event.preventDefault();
    void cancelRecalledMessageEdit();
    return;
  }
  if (branchesOpen.value) {
    if (!sending.value) {
      event.preventDefault();
      void cancelBranchNavigator();
      return;
    }
  }
  if (editingGoal.value) {
    event.preventDefault();
    void cancelGoalEdit();
    return;
  }
  if (selectedRuntime.value.queuedPromptEdit) {
    event.preventDefault();
    void cancelQueuedPromptEdit();
    return;
  }
  if (describedGoal.value) {
    event.preventDefault();
    describedGoal.value = null;
    return;
  }
  if (goalsOpen.value) {
    event.preventDefault();
    goalsOpen.value = false;
    return;
  }
  if (cronDraft.value) {
    event.preventDefault();
    cronDraft.value = null;
    return;
  }
  if (cronOpen.value) {
    event.preventDefault();
    cronOpen.value = false;
    return;
  }
  if (!sending.value) return;
  const now = Date.now();
  if (now - lastEscapeAt < 1000) {
    event.preventDefault();
    lastEscapeAt = 0;
    cancelTurn();
  } else {
    lastEscapeAt = now;
  }
}

function closeAutocomplete() {
  autocompleteController?.abort();
  autocompleteController = null;
  autocompleteRequest += 1;
  autocomplete.value = null;
  autocompleteSelection.value = 0;
}

async function refreshAutocomplete(element = composer.value) {
  if (!element || sending.value) {
    closeAutocomplete();
    return;
  }
  autocompleteController?.abort();
  const controller = new AbortController();
  autocompleteController = controller;
  const cursor = element.selectionStart ?? draft.value.length;
  const request = ++autocompleteRequest;
  autocomplete.value = null;
  autocompleteSelection.value = 0;
  const payload = {
    request_id: request,
    session_id: session.value?.id,
    before_cursor: draft.value.slice(0, cursor),
    after_cursor: draft.value.slice(cursor)
  };
  try {
    const result = await api("/api/completions", {
      method: "POST",
      body: JSON.stringify(payload),
      signal: controller.signal
    });
    if (request !== autocompleteRequest || controller.signal.aborted) return;
    if (!result) {
      autocomplete.value = null;
      return;
    }
    applyAutocompleteUpdate(result, request);
    if (result.recursive) {
      void streamRecursiveCompletions(payload, request, controller);
    }
  } catch (cause) {
    if (cause.name === "AbortError") return;
    if (request === autocompleteRequest) {
      autocomplete.value = null;
      autocompleteSelection.value = 0;
    }
  }
}

function applyAutocompleteUpdate(update, request) {
  const merged = mergeCompletionUpdate(
    autocomplete.value,
    autocompleteSelection.value,
    update,
    request
  );
  if (!merged.applied) return;
  autocomplete.value = merged.menu;
  autocompleteSelection.value = merged.selectedIndex;
}

async function streamRecursiveCompletions(payload, request, controller) {
  try {
    const response = await fetch("/api/completions/recursive", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      signal: controller.signal
    });
    if (!response.ok) {
      const body = await response.json().catch(() => ({}));
      throw new Error(
        body.error || `Completion search failed with ${response.status}`
      );
    }
    await consumeCompletionNdjson(response, (message) => {
      if (
        message.type === "update" &&
        request === autocompleteRequest &&
        !controller.signal.aborted
      ) {
        applyAutocompleteUpdate(message, request);
      }
    });
  } catch (cause) {
    if (cause.name !== "AbortError" && request === autocompleteRequest) {
      // Immediate directory results remain usable when recursion fails.
    }
  } finally {
    if (autocompleteController === controller) {
      autocompleteController = null;
    }
  }
}

function handleComposerInput(event) {
  resizeComposer();
  refreshAutocomplete(event.target);
}

function moveAutocomplete(delta) {
  const length = autocomplete.value?.items?.length ?? 0;
  if (!length) return;
  autocompleteSelection.value =
    (autocompleteSelection.value + delta + length) % length;
}

async function acceptAutocomplete(index = autocompleteSelection.value) {
  const menu = autocomplete.value;
  const item = menu?.items?.[index];
  const element = composer.value;
  if (!menu || !item || !element) return;

  const cursor = element.selectionStart ?? draft.value.length;
  const start = cursor - menu.replace_before.length;
  const end = cursor + menu.replace_after.length;
  if (
    start < 0 ||
    draft.value.slice(start, cursor) !== menu.replace_before ||
    draft.value.slice(cursor, end) !== menu.replace_after
  ) {
    await refreshAutocomplete(element);
    return;
  }

  draft.value =
    draft.value.slice(0, start) + item.replacement + draft.value.slice(end);
  const nextCursor = start + item.replacement.length;
  closeAutocomplete();
  await nextTick();
  element.focus();
  element.setSelectionRange(nextCursor, nextCursor);
  resizeComposer();
  if (item.kind === "directory") {
    await refreshAutocomplete(element);
  }
}

function completionLabel(item) {
  if (item.kind !== "file" && item.kind !== "directory") {
    return `/${item.name}`;
  }
  return item.display;
}

function handleComposerBlur() {
  window.setTimeout(closeAutocomplete, 100);
}

function handleComposerKey(event) {
  if (autocomplete.value?.items?.length) {
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveAutocomplete(-1);
      return;
    }
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveAutocomplete(1);
      return;
    }
    if (event.key === "PageUp") {
      event.preventDefault();
      moveAutocomplete(-5);
      return;
    }
    if (event.key === "PageDown") {
      event.preventDefault();
      moveAutocomplete(5);
      return;
    }
    if (
      event.key === "Tab" ||
      (event.key === "Enter" && !event.shiftKey && !event.altKey)
    ) {
      event.preventDefault();
      acceptAutocomplete();
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeAutocomplete();
      return;
    }
  }
  if (event.key === "ArrowUp" && draft.value === "" && !sending.value) {
    event.preventDefault();
    void recallLatestUserMessage();
    return;
  }
  if (event.key === "Enter" && !event.shiftKey && !event.altKey) {
    event.preventDefault();
    sendPrompt();
  }
}

function resizeComposer() {
  if (!composer.value) return;
  composer.value.style.height = "auto";
  composer.value.style.height = `${Math.min(composer.value.scrollHeight, 192)}px`;
}

async function clearComposer(options) {
  replacingAttachmentDraft = true;
  await composerDrafts.clear(options);
  draftAttachments.value = [];
  replacingAttachmentDraft = false;
  if (!options?.keepStored && attachmentDraftIdentity) {
    try {
      localStorage.removeItem(
        attachmentStorageKey(
          attachmentDraftIdentity.project,
          attachmentDraftIdentity.sessionId
        )
      );
    } catch {
      // Clearing the composer remains available without browser storage.
    }
  }
}

function finishAttachmentSend(sent, completed) {
  if (!completed || !sent.identity) return;
  try {
    localStorage.removeItem(
      attachmentStorageKey(sent.identity.project, sent.identity.sessionId)
    );
  } catch {
    // Sending succeeds independently of browser storage cleanup.
  }
}

async function sha256Hex(file) {
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

async function acquireBrowserAttachment(file, target) {
  if (file.size > 25 * 1024 * 1024) {
    throw new Error("Attachment exceeds the 25 MiB limit");
  }
  const sha256 = await sha256Hex(file);
  const preflight = await fetch("/api/attachments/preflight", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      project: target.project,
      session_id: target.sessionId,
      sha256
    })
  });
  if (preflight.ok) return preflight.json();
  if (preflight.status !== 404) {
    const body = await preflight.json().catch(() => ({}));
    throw new Error(body.error || `Attachment preflight failed with ${preflight.status}`);
  }
  const query = new URLSearchParams({
    project: target.project,
    session_id: target.sessionId,
    sha256,
    name: file.name || "pasted-image",
    mime_type: file.type || "application/octet-stream"
  });
  const upload = await fetch(`/api/attachments/upload?${query}`, {
    method: "POST",
    headers: { "Content-Type": file.type || "application/octet-stream" },
    body: file
  });
  const body = await upload.json().catch(() => ({}));
  if (!upload.ok) {
    throw new Error(body.error || `Attachment upload failed with ${upload.status}`);
  }
  return body;
}

async function insertBrowserFiles(files) {
  const target = {
    project: state.value?.project,
    sessionId: session.value?.id
  };
  if (!target.project || !target.sessionId || !files.length) return;
  uploadingAttachments.value = true;
  error.value = "";
  try {
    for (const file of files) {
      const attachment = await acquireBrowserAttachment(file, target);
      if (
        state.value?.project !== target.project ||
        session.value?.id !== target.sessionId
      ) {
        continue;
      }
      const element = composer.value;
      const start = element?.selectionStart ?? draft.value.length;
      const end = element?.selectionEnd ?? start;
      const previousDraft = draft.value;
      const inserted = insertAttachmentReference(
        previousDraft,
        start,
        end,
        attachment
      );
      replacingAttachmentDraft = true;
      draft.value = inserted.text;
      draftAttachments.value = [
        ...reconcileAttachmentBindings(
          previousDraft,
          inserted.text,
          draftAttachments.value
        ),
        inserted.binding
      ].sort((left, right) => left.start - right.start);
      replacingAttachmentDraft = false;
      await nextTick();
      resizeComposer();
      composer.value?.focus();
      composer.value?.setSelectionRange(inserted.cursor, inserted.cursor);
      persistAttachmentDraft();
    }
  } catch (cause) {
    error.value = `Could not attach file: ${cause.message}`;
  } finally {
    uploadingAttachments.value = false;
    if (attachmentInput.value) attachmentInput.value.value = "";
  }
}

function selectAttachments(event) {
  void insertBrowserFiles([...event.target.files]);
}

function pasteAttachments(event) {
  const files = [...(event.clipboardData?.files ?? [])];
  if (!files.length) return;
  event.preventDefault();
  void insertBrowserFiles(files);
}

function dropAttachments(event) {
  const files = [...(event.dataTransfer?.files ?? [])];
  if (!files.length) return;
  event.preventDefault();
  void insertBrowserFiles(files);
}

function timelineDescriptor(item) {
  return {
    key: item.key,
    estimate: estimateTimelineItemHeight(item),
    signature: timelineItemSignature(item),
    mounted: timelineRowElements.has(item.key)
  };
}

function timelineItemSignature(item) {
  if (item.type === "message") {
    return `message:${textShape(item.message.content)}`;
  }
  return visibleTurnEvents(item)
    .map((event) =>
      event.type === "message"
        ? `${event.key}:message:${textShape(event.message.content)}`
        : `${event.key}:activity:${event.activity.status}:${textShape(
            event.activity.detail
          )}:${expandedActivityKeys.value.has(event.key)}`
    )
    .join("|");
}

function textShape(content) {
  const text = content ?? "";
  const lineBreaks = (text.match(/\n/g) ?? []).length;
  return `${text.length}:${lineBreaks}:${text.slice(0, 24)}:${text.slice(-24)}`;
}

function estimateTimelineItemHeight(item) {
  if (item.type === "message") {
    return 58 + estimatedWrappedLines(item.message.content, 82) * 21;
  }
  let height = 54;
  if (turnHasCollapsibleProgress(item)) height += 32;
  for (const event of visibleTurnEvents(item)) {
    if (event.type === "activity") {
      height += expandedActivityKeys.value.has(event.key)
        ? 30 + estimatedWrappedLines(event.activity.detail, 72) * 16
        : 32;
    } else {
      height += 28 + estimatedWrappedLines(event.message.content, 78) * 19;
    }
  }
  if (activeAssistantTurnKey.value === item.key) height += 18;
  return Math.min(2_400, Math.max(72, height));
}

function estimatedWrappedLines(content, width) {
  return (content ?? "").split("\n").reduce(
    (total, line) => total + Math.max(1, Math.ceil(line.length / width)),
    0
  );
}

async function activateVirtualTimeline(
  sessionId,
  items,
  previousSessionId
) {
  const activation = ++virtualActivation;
  const switching = sessionId !== previousSessionId;
  if (switching) {
    saveVirtualSessionView(previousSessionId);
    disconnectTimelineRows();
  }

  let model = virtualTimelineStore.active();
  const sameTopology =
    !switching &&
    model &&
    model.keys.length === items.length &&
    (!items.length ||
      (model.keys[0] === items[0].key &&
        model.keys.at(-1) === items.at(-1).key));
  if (sameTopology) {
    const last = items.at(-1);
    if (last) model.updateItem(timelineDescriptor(last));
  } else {
    model = virtualTimelineStore.activate(
      sessionId,
      items.map(timelineDescriptor)
    );
  }
  if (!model) {
    virtualWindow.value = {
      start: 0,
      end: 0,
      top: 0,
      bottom: 0,
      total: 0
    };
    return;
  }

  updateVirtualWindow();
  await nextTick();
  if (activation !== virtualActivation) return;
  if (switching) {
    await restoreVirtualSessionView(sessionId, activation);
  } else if (autoScroll.value) {
    await scrollToBottom();
  } else {
    updateVirtualWindow();
  }
}

function saveVirtualSessionView(sessionId = session.value?.id) {
  const element = conversation.value;
  const model = virtualTimelineStore.active();
  if (!sessionId || !element || !model) return;
  virtualTimelineStore.saveView(sessionId, {
    anchor: model.anchorAt(conversationContentScrollTop(element)),
    followBottom: autoScroll.value
  });
}

async function restoreVirtualSessionView(sessionId, activation) {
  const element = conversation.value;
  const model = virtualTimelineStore.active();
  if (!element || !model || activation !== virtualActivation) return;
  const view = virtualTimelineStore.view(sessionId);
  autoScroll.value = view?.followBottom ?? true;
  updateVirtualWindow();
  await nextTick();
  if (activation !== virtualActivation) return;
  if (autoScroll.value) {
    element.scrollTop = element.scrollHeight;
  } else {
    element.scrollTop =
      TIMELINE_VERTICAL_PADDING + model.scrollTopForAnchor(view.anchor);
  }
  updateVirtualWindow();
}

function conversationContentScrollTop(element) {
  return Math.max(0, element.scrollTop - TIMELINE_VERTICAL_PADDING);
}

function updateVirtualWindow() {
  const element = conversation.value;
  const model = virtualTimelineStore.active();
  if (!model) return;
  virtualWindow.value = model.range(
    element ? conversationContentScrollTop(element) : 0,
    element?.clientHeight ?? 800,
    VIRTUAL_TIMELINE_OVERSCAN
  );
}

function handleConversationScroll() {
  if (conversation.value) {
    autoScroll.value = isScrolledToBottom(conversation.value);
    updateVirtualWindow();
  }
}

async function scrollToBottom() {
  if (!autoScroll.value) return;
  await nextTick();
  if (autoScroll.value && conversation.value) {
    conversation.value.scrollTop = conversation.value.scrollHeight;
    updateVirtualWindow();
  }
}

async function scrollToBranchMessage(
  nodeId,
  { revealOnly = false, current = () => true } = {}
) {
  await nextTick();
  await nextTick();
  if (!current()) return false;
  const item = timeline.value.find(
    (candidate) =>
      candidate.type === "message" && candidate.nodeId === nodeId
  );
  const model = virtualTimelineStore.active();
  const element = conversation.value;
  const index = item ? model?.indexByKey.get(item.key) : null;
  if (!item || !model || !element || index == null || !current()) {
    return false;
  }
  const contentScrollTop = conversationContentScrollTop(element);
  const nextContentScrollTop = revealOnly
    ? model.scrollTopToRevealIndex(
        index,
        contentScrollTop,
        element.clientHeight,
        16
      )
    : Math.max(0, model.offsetForIndex(index) - 16);
  if (Math.abs(nextContentScrollTop - contentScrollTop) < 1) return true;
  autoScroll.value = false;
  element.scrollTop =
    TIMELINE_VERTICAL_PADDING + nextContentScrollTop;
  updateVirtualWindow();
  return true;
}

function ensureTimelineResizeObserver() {
  if (timelineResizeObserver || typeof ResizeObserver === "undefined") return;
  timelineResizeObserver = new ResizeObserver((entries) => {
    measureTimelineRows(entries);
  });
}

function bindTimelineRow(key, element) {
  const previous = timelineRowElements.get(key);
  if (previous === element) return;
  if (previous) {
    timelineResizeObserver?.unobserve(previous);
    timelineElementKeys.delete(previous);
    timelineRowElements.delete(key);
  }
  if (!element) return;
  ensureTimelineResizeObserver();
  timelineRowElements.set(key, element);
  timelineElementKeys.set(element, key);
  timelineResizeObserver?.observe(element);
}

function disconnectTimelineRows() {
  for (const element of timelineRowElements.values()) {
    timelineResizeObserver?.unobserve(element);
  }
  timelineRowElements.clear();
  timelineElementKeys.clear();
}

function measureTimelineRows(entries) {
  const model = virtualTimelineStore.active();
  const element = conversation.value;
  if (!model || !element) return;
  const anchor = autoScroll.value
    ? null
    : model.anchorAt(conversationContentScrollTop(element));
  let changed = false;
  for (const entry of entries) {
    const key = timelineElementKeys.get(entry.target);
    if (!key) continue;
    changed =
      model.measure(key, entry.target.getBoundingClientRect().height).changed ||
      changed;
  }
  if (!changed) return;
  updateVirtualWindow();
  const activation = virtualActivation;
  void nextTick().then(() => {
    if (activation !== virtualActivation || !conversation.value) return;
    if (autoScroll.value) {
      conversation.value.scrollTop = conversation.value.scrollHeight;
    } else if (anchor) {
      conversation.value.scrollTop =
        TIMELINE_VERTICAL_PADDING + model.scrollTopForAnchor(anchor);
    }
    updateVirtualWindow();
  });
}

function invalidateVirtualMeasurements() {
  const model = virtualTimelineStore.active();
  const element = conversation.value;
  if (!model || !element) return;
  const anchor = autoScroll.value
    ? null
    : model.anchorAt(conversationContentScrollTop(element));
  if (!model.invalidateMeasurements()) return;
  updateVirtualWindow();
  const activation = virtualActivation;
  void nextTick().then(() => {
    if (activation !== virtualActivation || !conversation.value) return;
    if (autoScroll.value) {
      conversation.value.scrollTop = conversation.value.scrollHeight;
    } else if (anchor) {
      conversation.value.scrollTop =
        TIMELINE_VERTICAL_PADDING + model.scrollTopForAnchor(anchor);
    }
    updateVirtualWindow();
  });
}

function observeConversationSize() {
  if (!conversation.value || typeof ResizeObserver === "undefined") return;
  conversationResizeObserver?.disconnect();
  conversationWidth = conversation.value.getBoundingClientRect().width;
  conversationResizeObserver = new ResizeObserver(([entry]) => {
    const width = entry.contentRect.width;
    if (Math.abs(width - conversationWidth) < 1) return;
    conversationWidth = width;
    invalidateVirtualMeasurements();
  });
  conversationResizeObserver.observe(conversation.value);
}

function handleFontMetricsChange() {
  invalidateVirtualMeasurements();
}

async function copyMessage(content, key) {
  try {
    await navigator.clipboard.writeText(content);
    copiedMessageKey.value = key;
    window.clearTimeout(copiedMessageTimer);
    copiedMessageTimer = window.setTimeout(() => {
      if (copiedMessageKey.value === key) copiedMessageKey.value = "";
    }, 1600);
  } catch (cause) {
    error.value = `Could not copy the message: ${cause.message}`;
  }
}

function setActivityOverflow(key, overflowing) {
  if (overflowingActivityKeys.value.has(key) === overflowing) return;
  const next = new Set(overflowingActivityKeys.value);
  if (overflowing) next.add(key);
  else next.delete(key);
  overflowingActivityKeys.value = next;
}

function measureActivityDetail(key, element) {
  if (!element || expandedActivityKeys.value.has(key)) return;
  setActivityOverflow(key, element.scrollWidth > element.clientWidth);
}

function bindActivityDetail(key, element) {
  if (activityDetailElements.get(key) === element) return;
  activityDetailObservers.get(key)?.disconnect();
  activityDetailObservers.delete(key);
  activityDetailElements.delete(key);

  if (!element) {
    setActivityOverflow(key, false);
    return;
  }

  activityDetailElements.set(key, element);
  const observer = new ResizeObserver(() => {
    measureActivityDetail(key, element);
  });
  observer.observe(element);
  activityDetailObservers.set(key, observer);
  measureActivityDetail(key, element);
}

async function toggleActivityDetail(key) {
  const next = new Set(expandedActivityKeys.value);
  const collapsing = next.delete(key);
  if (!collapsing) next.add(key);
  expandedActivityKeys.value = next;
  await nextTick();
  if (collapsing) {
    measureActivityDetail(key, activityDetailElements.get(key));
  }
}

function shortId(id) {
  return id?.slice(0, 8) ?? "";
}

onMounted(() => {
  window.addEventListener("popstate", handleHistoryNavigation);
  window.addEventListener("keydown", handleGlobalKeydown);
  observeConversationSize();
  document.fonts?.addEventListener?.("loadingdone", handleFontMetricsChange);
  void document.fonts?.ready?.then(handleFontMetricsChange);
  void loadState({ resumeGoal: true }).then(() => connectSessionStream());
});
onBeforeUnmount(() => {
  if (cronRefreshTimer !== null) window.clearInterval(cronRefreshTimer);
  saveVirtualSessionView();
  disconnectTimelineRows();
  timelineResizeObserver?.disconnect();
  conversationResizeObserver?.disconnect();
  document.fonts?.removeEventListener?.(
    "loadingdone",
    handleFontMetricsChange
  );
  composerDrafts.persist();
  autocompleteController?.abort();
  sessionStreamController?.abort();
  window.clearTimeout(sessionStreamRetry);
  window.removeEventListener("popstate", handleHistoryNavigation);
  window.removeEventListener("keydown", handleGlobalKeydown);
  window.clearTimeout(copiedMessageTimer);
  for (const observer of activityDetailObservers.values()) observer.disconnect();
  discardRecording = true;
  sendAfterTranscription = false;
  if (mediaRecorder?.state === "recording") mediaRecorder.stop();
  stopMicrophoneTracks();
});
</script>

<template>
  <div class="app-shell h-dvh overflow-hidden bg-ink text-zinc-200">
    <div
      v-if="sidebarOpen"
      class="fixed inset-0 z-30 bg-black/60 backdrop-blur-sm lg:hidden"
      @click="sidebarOpen = false"
    />

    <div
      v-if="cronOpen"
      class="fixed inset-0 z-50 grid place-items-center bg-black/70 p-4 backdrop-blur-sm"
      @click.self="cronOpen = false"
    >
      <section class="flex max-h-[88vh] w-full max-w-4xl flex-col overflow-hidden rounded-xl border border-amber-400/20 bg-[#15131a] shadow-2xl shadow-black/60">
        <header class="flex items-center gap-3 border-b border-white/7 px-4 py-3">
          <CalendarClock class="size-4 text-amber-300" aria-hidden="true" />
          <div class="min-w-0 flex-1">
            <h2 class="text-sm font-semibold text-zinc-100">Scheduled agent tasks</h2>
            <p class="mt-0.5 truncate font-mono text-[9px] text-zinc-600">{{ cron?.path }}</p>
          </div>
          <span class="rounded px-2 py-1 text-[9px] font-semibold uppercase" :class="cron?.daemon === 'running' ? 'bg-emerald-400/10 text-emerald-300' : 'bg-amber-400/10 text-amber-300'">{{ cron?.daemon }}</span>
          <button v-if="cron?.installation?.status === 'not_installed'" class="rounded-md px-3 py-1.5 text-xs text-emerald-300 hover:bg-emerald-400/10" @click="installCron">Install service</button>
          <button v-else-if="!['running_unmanaged'].includes(cron?.installation?.status)" class="rounded-md px-3 py-1.5 text-xs text-zinc-500 hover:bg-red-500/10 hover:text-red-300" @click="uninstallCron">Uninstall service</button>
          <button class="rounded-md px-3 py-1.5 text-xs text-amber-300 hover:bg-amber-400/10" @click="editCronJob()">Add</button>
          <button class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-white/5 hover:text-zinc-200" aria-label="Close scheduled tasks" @click="cronOpen = false"><X class="size-4" /></button>
        </header>
        <div class="min-h-0 space-y-2 overflow-y-auto p-3">
          <label class="flex items-center gap-2 rounded-lg border border-white/7 bg-white/[0.02] px-3 py-2 text-[10px] text-zinc-500">
            Default timezone
            <input :value="cron?.document?.timezone" class="control min-w-0 flex-1 font-mono" @change="setCronTimezone($event.target.value)" />
          </label>
          <p v-if="!cronJobs.length" class="px-3 py-10 text-center text-xs text-zinc-600">No scheduled tasks. Add one here or ask an agent to propose one.</p>
          <article v-for="view in cronJobs" :key="view.id" class="rounded-lg border border-white/7 bg-white/[0.02] p-3">
            <div class="flex items-start gap-3">
              <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-2">
                  <span class="font-mono text-xs font-semibold text-amber-200">{{ view.id }}</span>
                  <span v-if="!view.job.enabled" class="rounded bg-zinc-500/10 px-1.5 py-0.5 text-[8px] uppercase text-zinc-500">paused</span>
                  <span v-if="view.state.one_time_status" class="rounded bg-white/5 px-1.5 py-0.5 text-[8px] uppercase text-zinc-400">{{ view.state.one_time_status }}</span>
                </div>
                <p class="mt-1 text-[10px] text-zinc-500">{{ view.description }}</p>
                <p class="mt-1 truncate text-[10px] text-zinc-400">{{ view.job.prompt }}</p>
                <p class="mt-1 truncate font-mono text-[9px] text-zinc-650">{{ view.job.project }} · {{ view.job.model }} · {{ view.job.overlap }}</p>
              </div>
              <button class="grid size-7 place-items-center rounded-md text-zinc-500 hover:bg-emerald-400/10 hover:text-emerald-300" title="Run now" @click="runCronJob(view)"><Play class="size-3.5" /></button>
              <button class="grid size-7 place-items-center rounded-md text-zinc-500 hover:bg-amber-400/10 hover:text-amber-300" :title="view.job.enabled ? 'Pause' : 'Resume'" @click="toggleCronJob(view)"><Pause v-if="view.job.enabled" class="size-3.5" /><Play v-else class="size-3.5" /></button>
              <button class="grid size-7 place-items-center rounded-md text-zinc-500 hover:bg-white/5 hover:text-zinc-200" title="Edit" @click="editCronJob(view)"><Pencil class="size-3.5" /></button>
              <button class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-red-500/10 hover:text-red-300" title="Delete" @click="deleteCronJob(view)"><Trash2 class="size-3.5" /></button>
            </div>
            <div v-if="view.next_occurrences.length" class="mt-3 flex flex-wrap gap-1.5">
              <span v-for="next in view.next_occurrences" :key="next" class="rounded bg-amber-400/5 px-2 py-1 font-mono text-[8px] text-amber-200/70">{{ formatCronTime(next) }}</span>
            </div>
            <div v-if="view.state.occurrences.length" class="mt-3 space-y-1 border-t border-white/5 pt-2">
              <button v-for="run in [...view.state.occurrences].reverse().slice(0, 8)" :key="run.sequence" class="flex w-full items-center gap-2 rounded px-2 py-1 text-left hover:bg-white/3" :disabled="!run.session_id" @click="run.session_id && (cronOpen = false, resumeSession(view.job.project, run.session_id))">
                <span class="w-8 font-mono text-[9px] text-zinc-600">#{{ run.sequence }}</span>
                <span class="w-28 text-[9px] font-semibold uppercase" :class="{'text-emerald-300': run.status === 'completed', 'text-red-300': ['failed', 'timed_out'].includes(run.status), 'text-amber-300': ['queued', 'running'].includes(run.status), 'text-zinc-500': run.status === 'skipped_overlap'}">{{ run.status }}</span>
                <span class="min-w-0 flex-1 truncate text-[10px] text-zinc-400">{{ run.last_message || run.error || 'No result yet' }}</span>
                <span class="font-mono text-[8px] text-zinc-650">{{ formatCronTime(run.scheduled_at) }}</span>
              </button>
            </div>
          </article>
        </div>
      </section>
    </div>

    <div
      v-if="cronDraft"
      class="fixed inset-0 z-[60] grid place-items-center bg-black/75 p-4 backdrop-blur-sm"
      @click.self="cronDraft = null"
    >
      <section class="max-h-[90vh] w-full max-w-2xl space-y-3 overflow-y-auto rounded-xl border border-amber-400/20 bg-[#15131a] p-4">
        <h2 class="text-sm font-semibold text-amber-200">Scheduled task</h2>
        <div class="grid gap-3 sm:grid-cols-2">
          <label class="block text-[10px] text-zinc-500">ID<input v-model="cronDraftId" :disabled="Boolean(cronDraftOriginalId)" class="control mt-1 w-full disabled:opacity-50" /></label>
          <label class="block text-[10px] text-zinc-500">Schedule<input v-model="cronDraft.schedule" placeholder="0 9 * * * or @at 2030-01-02T03:04:05+01:00" class="control mt-1 w-full font-mono" /></label>
          <label class="block text-[10px] text-zinc-500 sm:col-span-2">Project<input v-model="cronDraft.project" class="control mt-1 w-full font-mono" /></label>
          <label class="block text-[10px] text-zinc-500">Provider<input v-model="cronDraft.provider" class="control mt-1 w-full" /></label>
          <label class="block text-[10px] text-zinc-500">Model<input v-model="cronDraft.model" class="control mt-1 w-full" /></label>
          <label class="block text-[10px] text-zinc-500">Reasoning<input v-model="cronDraft.reasoning" class="control mt-1 w-full" /></label>
          <label class="block text-[10px] text-zinc-500">Speed/service tier<input v-model="cronDraft.speed" class="control mt-1 w-full" /></label>
          <label class="block text-[10px] text-zinc-500">Timezone<input v-model="cronDraft.timezone" class="control mt-1 w-full" /></label>
          <label class="block text-[10px] text-zinc-500">Overlap<select v-model="cronDraft.overlap" class="control mt-1 w-full"><option value="skip">skip</option><option value="queue">queue latest</option></select></label>
          <label class="block text-[10px] text-zinc-500">Timeout seconds<input v-model.number="cronDraft.timeout_seconds" type="number" min="1" placeholder="No timeout" class="control mt-1 w-full" /></label>
          <label class="flex items-end gap-2 pb-2 text-[10px] text-zinc-500"><input v-model="cronDraft.enabled" type="checkbox" /> Enabled</label>
          <label class="block text-[10px] text-zinc-500 sm:col-span-2">Source session ID<input v-model="cronDraft.source_session_id" placeholder="Optional lineage link" class="control mt-1 w-full font-mono" /></label>
          <label class="block text-[10px] text-zinc-500 sm:col-span-2">Prompt<textarea v-model="cronDraft.prompt" rows="6" class="control mt-1 w-full resize-y"></textarea></label>
        </div>
        <div class="flex justify-end gap-2 pt-2"><button class="rounded px-3 py-1.5 text-xs text-zinc-500 hover:bg-white/5" @click="cronDraft = null">Cancel</button><button class="rounded bg-amber-300 px-3 py-1.5 text-xs font-semibold text-amber-950" @click="saveCronJob">Save</button></div>
      </section>
    </div>

    <div
      v-if="providersOpen"
      class="fixed inset-0 z-50 grid place-items-center bg-black/70 p-4 backdrop-blur-sm"
      @click.self="providersOpen = false"
    >
      <section class="flex max-h-[80vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-cyan-400/20 bg-[#15131a] shadow-2xl shadow-black/60">
        <header class="flex items-center gap-3 border-b border-white/7 px-4 py-3">
          <Settings class="size-4 text-cyan-300" aria-hidden="true" />
          <div class="flex-1">
            <h2 class="text-sm font-semibold text-zinc-100">Providers</h2>
            <p class="mt-0.5 text-[10px] text-zinc-600">API keys are saved in ~/.config/codecrab/config.toml.</p>
          </div>
          <button class="rounded-md px-3 py-1.5 text-xs text-cyan-300 hover:bg-cyan-400/10" @click="editProvider()">Add</button>
          <button class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-white/5 hover:text-zinc-200" @click="providersOpen = false"><X class="size-4" /></button>
        </header>
        <div class="min-h-0 overflow-y-auto p-2">
          <article v-for="provider in providers" :key="provider.name" class="mb-1 flex items-center gap-3 rounded-lg px-3 py-2.5 hover:bg-white/3">
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2 text-xs font-semibold text-zinc-200">
                {{ provider.name }}
                <span v-if="provider.active" class="rounded bg-cyan-400/10 px-1.5 py-0.5 text-[8px] uppercase text-cyan-300">active</span>
              </div>
              <div class="mt-1 truncate font-mono text-[9px] text-zinc-600">{{ provider.model }} · {{ provider.auth }} · key {{ provider.api_key_configured ? 'configured' : 'none' }} · {{ provider.base_url }}</div>
            </div>
            <button v-if="!provider.active" class="rounded px-2 py-1 text-[10px] text-cyan-300 hover:bg-cyan-400/10" @click="useProvider(provider)">Use</button>
            <button class="grid size-7 place-items-center rounded-md text-zinc-500 hover:bg-white/5" @click="editProvider(provider)"><Pencil class="size-3.5" /></button>
            <button :disabled="provider.active" class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-red-500/10 hover:text-red-300 disabled:opacity-25" @click="deleteProvider(provider)"><Trash2 class="size-3.5" /></button>
          </article>
        </div>
      </section>
    </div>

    <div
      v-if="providerDraft"
      class="fixed inset-0 z-[60] grid place-items-center bg-black/75 p-4 backdrop-blur-sm"
      @click.self="providerDraft = null"
    >
      <section class="w-full max-w-lg space-y-3 rounded-xl border border-cyan-400/20 bg-[#15131a] p-4">
        <h2 class="text-sm font-semibold text-cyan-200">Provider profile</h2>
        <label class="block text-[10px] text-zinc-500">Name<input v-model="providerDraft.name" :disabled="providers.some(p => p.name === providerDraft.name)" class="control mt-1 w-full" /></label>
        <label class="block text-[10px] text-zinc-500">Base URL<input v-model="providerDraft.base_url" class="control mt-1 w-full" /></label>
        <label class="block text-[10px] text-zinc-500">Model<input v-model="providerDraft.model" class="control mt-1 w-full" /></label>
        <label class="block text-[10px] text-zinc-500">Authentication<select v-model="providerDraft.auth" class="control mt-1 w-full"><option value="auto">auto</option><option value="oauth">oauth</option><option value="api_key">api_key</option><option value="none">none</option></select></label>
        <label class="block text-[10px] text-zinc-500">API key<input v-model="providerDraft.api_key" type="password" autocomplete="new-password" placeholder="Leave empty to keep the current key" class="control mt-1 w-full" /></label>
        <label v-if="providerDraft.api_key_configured" class="flex items-center gap-2 text-[10px] text-zinc-500"><input v-model="providerDraft.clear_api_key" type="checkbox" /> Remove configured API key</label>
        <div class="flex justify-end gap-2 pt-2"><button class="rounded px-3 py-1.5 text-xs text-zinc-500 hover:bg-white/5" @click="providerDraft = null">Cancel</button><button class="rounded bg-cyan-300 px-3 py-1.5 text-xs font-semibold text-cyan-950" @click="saveProvider">Save</button></div>
      </section>
    </div>

    <div
      v-if="goalsOpen"
      class="fixed inset-0 z-50 grid place-items-center bg-black/70 p-4 backdrop-blur-sm"
      @click.self="goalsOpen = false"
    >
      <section class="flex max-h-[80vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-violet-400/20 bg-[#15131a] shadow-2xl shadow-black/60">
        <header class="flex items-center gap-3 border-b border-white/7 px-4 py-3">
          <ListTodo class="size-4 text-violet-300" aria-hidden="true" />
          <div class="min-w-0 flex-1">
            <h2 class="text-sm font-semibold text-zinc-100">Goals</h2>
            <p class="mt-0.5 text-[10px] text-zinc-600">
              One active goal per session. Activating another pauses the current one.
            </p>
          </div>
          <button
            class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-white/5 hover:text-zinc-200"
            aria-label="Close goals"
            title="Close"
            @click="goalsOpen = false"
          >
            <X class="size-4" aria-hidden="true" />
          </button>
        </header>
        <div class="min-h-0 overflow-y-auto p-2">
          <p
            v-if="!goals.length"
            class="px-3 py-8 text-center text-xs text-zinc-600"
          >
            Create a goal with <span class="font-mono text-violet-300">/goal objective</span>.
          </p>
          <article
            v-for="goal in goalHistory"
            :key="goal.id"
            class="mb-1 flex items-center gap-3 rounded-lg border border-transparent px-3 py-2.5 transition hover:border-white/7 hover:bg-white/3"
          >
            <span
              class="w-14 shrink-0 font-mono text-[9px] font-semibold uppercase tracking-wider"
              :class="{
                'text-violet-300': goal.status === 'active',
                'text-amber-300': goal.status === 'paused',
                'text-emerald-300': goal.status === 'completed',
                'text-red-300': goal.status === 'blocked'
              }"
            >
              {{ goalStatusLabel(goal.status) }}
            </span>
            <button
              class="min-w-0 flex-1 truncate text-left text-xs text-zinc-300 hover:text-white"
              title="Show complete goal"
              @click="describedGoal = goal"
            >
              {{ compactGoal(goal) }}
            </button>
            <button
              class="grid size-7 shrink-0 place-items-center rounded-md text-zinc-500 hover:bg-violet-400/10 hover:text-violet-300"
              :aria-label="goal.status === 'active' ? 'Pause goal' : 'Activate goal'"
              :title="goal.status === 'active' ? 'Pause goal' : 'Activate goal'"
              @click="toggleGoal(goal)"
            >
              <Pause
                v-if="goal.status === 'active'"
                class="size-3.5"
                aria-hidden="true"
              />
              <Play v-else class="size-3.5" aria-hidden="true" />
            </button>
            <button
              class="grid size-7 shrink-0 place-items-center rounded-md text-zinc-500 hover:bg-white/5 hover:text-zinc-200"
              aria-label="Edit goal"
              title="Edit goal"
              @click="beginGoalEdit(goal)"
            >
              <Pencil class="size-3.5" aria-hidden="true" />
            </button>
            <button
              class="grid size-7 shrink-0 place-items-center rounded-md text-zinc-600 hover:bg-red-500/10 hover:text-red-300"
              aria-label="Delete goal"
              title="Delete goal"
              @click="deleteGoal(goal)"
            >
              <Trash2 class="size-3.5" aria-hidden="true" />
            </button>
          </article>
        </div>
      </section>
    </div>

    <div
      v-if="describedGoal"
      class="fixed inset-0 z-[60] grid place-items-center bg-black/75 p-4 backdrop-blur-sm"
      @click.self="describedGoal = null"
    >
      <section class="flex max-h-[75vh] w-full max-w-xl flex-col overflow-hidden rounded-xl border border-violet-400/20 bg-[#15131a]">
        <header class="flex items-center gap-3 border-b border-white/7 px-4 py-3">
          <span class="flex-1 text-xs font-semibold text-violet-200">
            Goal description
          </span>
          <button
            class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-white/5 hover:text-zinc-200"
            aria-label="Close description"
            @click="describedGoal = null"
          >
            <X class="size-4" aria-hidden="true" />
          </button>
        </header>
        <div class="overflow-y-auto whitespace-pre-wrap px-5 py-4 text-sm leading-6 text-zinc-300">
          {{ describedGoal.objective }}
        </div>
      </section>
    </div>

    <div
      v-if="editingGoal"
      class="fixed inset-0 z-[70] grid place-items-center bg-black/75 p-4 backdrop-blur-sm"
      @click.self="cancelGoalEdit"
    >
      <section class="w-full max-w-xl rounded-xl border border-violet-400/20 bg-[#15131a] p-4 shadow-2xl">
        <label class="mb-2 block text-xs font-semibold text-violet-200" for="goal-editor">
          Edit goal
        </label>
        <textarea
          id="goal-editor"
          v-model="goalDraft"
          rows="10"
          maxlength="4000"
          class="w-full resize-y rounded-lg border border-white/10 bg-black/20 px-3 py-2 font-mono text-xs leading-5 text-zinc-200 outline-none focus:border-violet-400/35"
        />
        <div class="mt-3 flex items-center justify-between">
          <span class="font-mono text-[9px] text-zinc-600">
            {{ goalDraft.length }}/4000
          </span>
          <div class="flex gap-2">
            <button
              class="rounded-md px-3 py-1.5 text-xs text-zinc-500 hover:bg-white/5 hover:text-zinc-200"
              @click="cancelGoalEdit"
            >
              Cancel
            </button>
            <button
              class="rounded-md bg-violet-300 px-3 py-1.5 text-xs font-semibold text-violet-950 hover:bg-violet-200 disabled:opacity-40"
              :disabled="!goalDraft.trim()"
              @click="saveGoalEdit"
            >
              Save
            </button>
          </div>
        </div>
      </section>
    </div>

    <div
      v-if="projectPickerOpen"
      class="fixed inset-0 z-[80] grid place-items-center bg-black/75 p-3 backdrop-blur-sm sm:p-6"
      @click.self="projectPickerOpen = false"
    >
      <section class="flex max-h-[90dvh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-cyan-400/20 bg-[#15171a] shadow-2xl shadow-black/60">
        <header class="flex shrink-0 items-center gap-3 border-b border-white/7 px-4 py-3">
          <FolderOpen class="size-4 text-cyan-300" aria-hidden="true" />
          <div class="min-w-0 flex-1">
            <h2 class="text-sm font-semibold text-zinc-100">Open project on server</h2>
            <p class="mt-0.5 text-[10px] text-zinc-600">Browse directories on the machine running CodeCrab.</p>
          </div>
          <button
            class="grid size-7 place-items-center rounded-md text-zinc-600 hover:bg-white/5 hover:text-zinc-200"
            aria-label="Close project picker"
            @click="projectPickerOpen = false"
          >
            <X class="size-4" aria-hidden="true" />
          </button>
        </header>

        <form class="flex shrink-0 gap-2 border-b border-white/6 p-3" @submit.prevent="browseDirectory()">
          <input
            v-model="directoryPathDraft"
            class="min-w-0 flex-1 rounded-md border border-white/8 bg-black/20 px-3 py-2 font-mono text-xs text-zinc-300 outline-none focus:border-cyan-400/30"
            aria-label="Server directory path"
            autocomplete="off"
          />
          <button
            class="rounded-md border border-white/8 px-3 py-2 text-xs text-zinc-300 hover:bg-white/5 disabled:opacity-40"
            :disabled="projectPickerLoading"
          >
            Go
          </button>
        </form>

        <div v-if="projectPickerError" class="mx-3 mt-3 rounded-md border border-red-500/20 bg-red-500/8 px-3 py-2 text-xs text-red-300">
          {{ projectPickerError }}
        </div>

        <div class="min-h-0 flex-1 overflow-y-auto p-2">
          <div v-if="projectPickerLoading && !directoryListing" class="grid min-h-40 place-items-center text-xs text-zinc-600">
            <LoaderCircle class="size-4 animate-spin" aria-hidden="true" />
          </div>
          <template v-else-if="directoryListing">
            <div v-if="directoryListing.roots.length > 1" class="mb-2 flex flex-wrap gap-1 px-1">
              <button
                v-for="root in directoryListing.roots"
                :key="root"
                class="rounded border border-white/8 px-2 py-1 font-mono text-[10px] text-zinc-500 hover:bg-white/5 hover:text-zinc-200"
                @click="browseDirectory(root)"
              >
                {{ root }}
              </button>
            </div>
            <button
              v-if="directoryListing.parent"
              class="flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left text-xs text-zinc-500 hover:bg-white/5 hover:text-zinc-200"
              @click="browseDirectory(directoryListing.parent)"
            >
              <ChevronLeft class="size-3.5" aria-hidden="true" />
              Parent directory
            </button>
            <button
              v-for="entry in directoryListing.directories"
              :key="entry.path"
              class="flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left hover:bg-white/5"
              :title="entry.path"
              @click="browseDirectory(entry.path)"
            >
              <Folder class="size-3.5 shrink-0 text-cyan-400" aria-hidden="true" />
              <span class="min-w-0 flex-1 truncate text-xs text-zinc-300">{{ entry.name }}</span>
              <ChevronLeft class="size-3 rotate-180 text-zinc-700" aria-hidden="true" />
            </button>
            <p v-if="!directoryListing.directories.length" class="px-3 py-8 text-center text-xs text-zinc-600">
              This directory has no child directories.
            </p>
          </template>
        </div>

        <footer class="shrink-0 border-t border-white/7 p-3">
          <form class="mb-3 flex gap-2" @submit.prevent="createDirectory">
            <input
              v-model="newDirectoryName"
              class="min-w-0 flex-1 rounded-md border border-white/8 bg-black/20 px-3 py-2 text-xs text-zinc-300 outline-none focus:border-cyan-400/30"
              placeholder="New directory name"
              aria-label="New directory name"
            />
            <button
              class="rounded-md border border-white/8 px-3 py-2 text-xs text-zinc-300 hover:bg-white/5 disabled:opacity-40"
              :disabled="projectPickerLoading || !newDirectoryName.trim() || !directoryListing"
            >
              Create
            </button>
          </form>
          <div class="flex flex-col-reverse gap-2 sm:flex-row sm:items-center sm:justify-between">
            <p class="min-w-0 truncate font-mono text-[9px] text-zinc-600" :title="directoryListing?.path">
              {{ directoryListing?.path }}
            </p>
            <button
              class="shrink-0 rounded-md bg-cyan-300 px-4 py-2 text-xs font-semibold text-cyan-950 hover:bg-cyan-200 disabled:opacity-40"
              :disabled="projectPickerLoading || !directoryListing"
              @click="openProject()"
            >
              Open this project
            </button>
          </div>
        </footer>
      </section>
    </div>

    <aside
      class="app-sidebar fixed inset-y-0 left-0 z-40 flex w-72 -translate-x-full flex-col border-r bg-panel transition-transform duration-200 lg:translate-x-0"
      :class="{ 'translate-x-0': sidebarOpen }"
    >
      <div class="flex h-14 items-center gap-3 border-b border-white/7 px-4">
        <div class="brand-mark grid size-7 place-items-center rounded-md bg-coral text-sm font-black text-black">
          C
        </div>
        <span class="text-sm font-semibold tracking-tight text-white">CodeCrab</span>
      </div>

      <div class="px-3 py-3.5">
        <button
          class="project-add-button flex w-full items-center justify-center gap-2 rounded-lg border px-3 py-2.5 text-xs font-semibold transition"
          @click="showProjectPicker"
        >
          <FolderPlus class="size-4" aria-hidden="true" />
          Add or create project
        </button>
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-4" aria-label="Projects and sessions">
        <p
          v-if="!projects.length"
          class="px-2 py-3 text-xs leading-5 text-zinc-600"
        >
          No projects are open. Add a directory from the backend host to begin.
        </p>
        <section
          v-for="project in projects"
          :key="project.root"
          class="mb-1"
          :data-project-root="project.root"
        >
          <div
            class="sidebar-project-row group flex w-full items-center rounded-lg transition"
            :class="{ 'is-active': project.root === state?.project }"
          >
            <button
              class="flex min-w-0 flex-1 items-center gap-2 px-2 py-2 text-left"
              :title="project.root"
              :aria-expanded="projectExpanded(project.root)"
              @click="toggleProject(project.root)"
            >
              <FolderOpen
                v-if="projectExpanded(project.root)"
                class="size-3.5 shrink-0 text-cyan-400"
                aria-hidden="true"
              />
              <Folder
                v-else
                class="size-3.5 shrink-0 text-zinc-500"
                aria-hidden="true"
              />
              <span class="min-w-0 flex-1">
                <span class="block truncate text-[13px] font-semibold text-zinc-300 group-hover:text-white">
                  {{ projectName(project.root) }}
                </span>
                <span class="mt-0.5 block truncate font-mono text-[9px] text-zinc-500">
                  {{ project.root }}
                </span>
              </span>
              <span class="font-mono text-[10px] text-zinc-500">
                {{ project.sessions.length }}
              </span>
            </button>
            <button
              class="mr-1 grid size-7 shrink-0 place-items-center rounded text-zinc-500 transition hover:bg-coral/10 hover:text-coral"
              :aria-label="`New session in ${projectName(project.root)}`"
              title="New session in this project"
              @click.stop="newSession(project.root)"
            >
              <Plus class="size-3.5" aria-hidden="true" />
            </button>
          </div>

          <div
            v-if="projectExpanded(project.root)"
            class="sidebar-session-list ml-3 pl-2"
          >
            <p
              v-if="!project.sessions.length"
              class="px-2 py-2 text-[10px] leading-4 text-zinc-600"
            >
              No sessions yet. Use + on this project to create one.
            </p>
            <div
              v-for="item in visibleProjectSessions(project)"
              :key="item.id"
              class="sidebar-session-row group/session flex w-full items-center transition"
              :class="{ 'is-current': isCurrentSession(project, item) }"
              :style="{ paddingLeft: `${item.depth * 0.75}rem` }"
              :data-session-depth="item.depth"
            >
              <button
                v-if="item.descendant_count > 0"
                class="session-disclosure grid size-7 shrink-0 place-items-center rounded text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                :aria-label="`${sessionCollapsed(item.id) ? 'Expand' : 'Collapse'} ${item.title || 'session'}`"
                :aria-expanded="!sessionCollapsed(item.id)"
                @click.stop="toggleSession(item.id)"
              >
                <ChevronRight
                  v-if="sessionCollapsed(item.id)"
                  class="size-3.5"
                  aria-hidden="true"
                />
                <ChevronDown v-else class="size-3.5" aria-hidden="true" />
              </button>
              <span v-else class="size-7 shrink-0" aria-hidden="true" />
              <button
                class="flex min-w-0 flex-1 items-center gap-2 py-2 pr-2 text-left"
                @click="resumeSession(project.root, item.id)"
              >
                <span
                  v-if="isCurrentSession(project, item)"
                  class="current-session-dot absolute left-0 size-1.5 rounded-full bg-coral"
                  aria-hidden="true"
                />
                <CalendarClock
                  v-if="item.scheduled_run"
                  class="size-3.5 shrink-0 text-amber-400"
                  :title="`Scheduled run ${item.scheduled_run.job_id} #${item.scheduled_run.occurrence}`"
                  aria-label="Scheduled run"
                />
                <span class="min-w-0 flex-1 truncate text-[13px] text-zinc-300 group-hover/session:text-white">
                  {{ item.title || "New session" }}
                </span>
                <span
                  v-if="sessionCollapsed(item.id)"
                  class="shrink-0 rounded bg-white/5 px-1 font-mono text-[9px] text-zinc-500"
                  :title="`${item.descendant_count} hidden descendants`"
                >
                  +{{ item.descendant_count }}
                </span>
                <span
                  v-else-if="item.depth === 0 && item.parent_session_id"
                  class="shrink-0 font-mono text-[9px] text-zinc-500"
                  :title="`Parent session ${item.parent_session_id}`"
                >
                  child of {{ shortId(item.parent_session_id) }}
                </span>
                <span
                  v-if="workerLifecycle(item.id) && workerLifecycle(item.id) !== 'idle'"
                  class="size-1.5 shrink-0 rounded-full"
                  :class="{
                    'animate-pulse bg-cyan-400': workerLifecycle(item.id) === 'running',
                    'bg-red-400': workerLifecycle(item.id) === 'failed'
                  }"
                  :title="workerLifecycle(item.id)"
                />
              </button>
              <button
                class="mr-1 grid size-8 shrink-0 place-items-center rounded-md text-zinc-400 transition hover:bg-red-500/12 hover:text-red-300 disabled:pointer-events-none disabled:opacity-30"
                :disabled="runtimeFor(item.id).sending"
                :aria-label="`Delete session ${item.title || shortId(item.id)}`"
                title="Delete session"
                @click.stop="deleteSession(project.root, item.id)"
              >
                <Trash2 class="size-3.5" aria-hidden="true" />
              </button>
            </div>
          </div>
        </section>
      </div>
    </aside>

    <main class="flex h-full min-w-0 flex-col lg:pl-72">
      <header class="app-header flex h-14 shrink-0 items-center gap-3 border-b bg-ink/90 px-4 backdrop-blur">
        <button
          class="grid size-8 place-items-center rounded-md text-zinc-500 hover:bg-white/5 hover:text-white lg:hidden"
          aria-label="Open sessions"
          @click="sidebarOpen = true"
        >
          <Menu class="size-4" aria-hidden="true" />
        </button>

        <div class="min-w-0">
          <h1 class="truncate text-[13px] font-semibold text-zinc-100">
            {{ session?.title || "No session" }}
          </h1>
          <p class="mt-0.5 truncate font-mono text-[10px] text-zinc-400">
            {{ state?.project }}
          </p>
        </div>

        <div class="ml-auto flex items-center gap-2">
          <button
            v-if="session"
            class="grid size-8 place-items-center rounded-md transition hover:bg-white/5 hover:text-zinc-300 disabled:cursor-not-allowed disabled:opacity-30"
            :class="branchesOpen ? 'text-coral' : 'text-zinc-500'"
            :disabled="
              sending ||
              Boolean(editingMessageNode) ||
              Boolean(recalledMessageNode) ||
              !branchNodes.length
            "
            title="Browse conversation branches"
            aria-label="Browse conversation branches"
            :aria-pressed="branchesOpen"
            @click="branchesOpen ? cancelBranchNavigator() : openBranchNavigator()"
          >
            <GitBranch class="size-4" aria-hidden="true" />
          </button>
          <button class="grid size-8 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-amber-300" title="Scheduled tasks" aria-label="Manage scheduled tasks" @click="openCron"><CalendarClock class="size-4" /></button>
          <button class="grid size-8 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200" title="Providers" aria-label="Manage providers" @click="providersOpen = true"><Settings class="size-4" /></button>
        </div>
      </header>

      <div class="flex min-h-0 flex-1">
      <div
        ref="conversation"
        class="conversation-viewport min-h-0 flex-1 overflow-y-auto"
        data-testid="conversation-viewport"
        @scroll.passive="handleConversationScroll"
      >
        <div v-if="loading" class="grid h-full place-items-center">
          <div class="flex items-center gap-2 text-xs text-zinc-600">
            <span class="size-1.5 animate-pulse rounded-full bg-coral" />
            Loading workspace
          </div>
        </div>

        <div v-else-if="!timeline.length" class="h-full" />

        <div
          v-else
          class="mx-auto max-w-3xl px-4 py-6 sm:px-8"
          :data-timeline-mounted="virtualTimelineItems.length"
          :data-timeline-total="timeline.length"
        >
          <div
            :style="{ height: `${virtualWindow.top}px` }"
            aria-hidden="true"
          />
          <div
            v-for="item in virtualTimelineItems"
            :key="item.key"
            :ref="(element) => bindTimelineRow(item.key, element)"
            data-timeline-row
          >
            <article
              v-if="item.type === 'message'"
              class="message-row group"
              :class="{
                'branch-message-highlight':
                  matchesMessageNode(hoveredBranchNode, item.nodeId)
              }"
            >
              <div
                class="user-badge grid size-7 shrink-0 place-items-center rounded-md text-[10px] font-bold text-zinc-200"
              >
                U
              </div>
              <div class="min-w-0 flex-1">
                <template
                  v-if="
                    matchesMessageNode(editingMessageNode, item.nodeId)
                  "
                >
                  <textarea
                    ref="messageEditor"
                    v-model="messageEditDraft"
                    class="message-editor"
                    rows="3"
                    aria-label="Edit message"
                    @keydown="handleMessageEditorKey"
                  />
                  <div class="mt-2 flex justify-end gap-1">
                    <button
                      type="button"
                      class="grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                      title="Cancel edit"
                      aria-label="Cancel edit"
                      @click="cancelMessageEdit"
                    >
                      <X class="size-3.5" aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      class="grid size-7 place-items-center rounded-md bg-coral/12 text-coral transition hover:bg-coral/20 disabled:opacity-30"
                      :disabled="!messageEditDraft.trim()"
                      title="Create branch from edited message"
                      aria-label="Create branch from edited message"
                      @click="saveMessageEdit"
                    >
                      <Check class="size-3.5" aria-hidden="true" />
                    </button>
                  </div>
                </template>
                <template v-else>
                  <div
                    class="message-content"
                    :title="formatEventTimestamp(item.message.created_at)"
                  >
                    {{ item.message.content }}
                  </div>
                  <div class="mt-1 flex justify-end gap-1">
                    <button
                      type="button"
                      class="grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                      :disabled="sending || branchSelecting"
                      title="Edit message"
                      aria-label="Edit message"
                      @click="beginMessageEdit(item)"
                    >
                      <Pencil class="size-3.5" aria-hidden="true" />
                    </button>
                    <button
                      type="button"
                      class="grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                      :aria-label="
                        copiedMessageKey === item.key
                          ? 'Message copied'
                          : 'Copy message'
                      "
                      :title="
                        copiedMessageKey === item.key
                          ? 'Copied'
                          : 'Copy message'
                      "
                      @click="copyMessage(item.message.content, item.key)"
                    >
                      <Check
                        v-if="copiedMessageKey === item.key"
                        class="size-3 text-emerald-400"
                        aria-hidden="true"
                      />
                      <Copy v-else class="size-3.5" aria-hidden="true" />
                    </button>
                  </div>
                </template>
              </div>
            </article>

            <article v-else class="message-row group">
              <div class="assistant-badge grid size-7 shrink-0 place-items-center rounded-md text-[10px] font-bold text-coral">
                C
              </div>
              <div class="min-w-0 flex-1">
                <div
                  v-if="turnHasCollapsibleProgress(item)"
                  class="turn-summary"
                >
                  <span class="min-w-0 flex-1 truncate">{{
                    turnSummary(item)
                  }}</span>
                  <button
                    type="button"
                    class="grid size-6 shrink-0 place-items-center rounded text-zinc-600 transition hover:bg-white/5 hover:text-zinc-300"
                    :aria-expanded="turnIsExpanded(item)"
                    :aria-label="
                      turnIsExpanded(item)
                        ? 'Collapse turn progress'
                        : 'Expand turn progress'
                    "
                    :title="
                      turnIsExpanded(item)
                        ? 'Collapse progress'
                        : 'Expand progress'
                    "
                    @click="toggleTurnProgress(item.key)"
                  >
                    <ChevronUp
                      v-if="turnIsExpanded(item)"
                      class="size-3.5"
                      aria-hidden="true"
                    />
                    <ChevronDown
                      v-else
                      class="size-3.5"
                      aria-hidden="true"
                    />
                  </button>
                </div>
                <template
                  v-for="(event, eventIndex) in visibleTurnEvents(item)"
                  :key="event.key"
                >
                  <div
                    v-if="event.type === 'activity'"
                    class="activity-row"
                    :class="[
                      `activity-${event.activity.kind}`,
                      {
                        'activity-row-expanded': expandedActivityKeys.has(
                          event.key
                        )
                      }
                    ]"
                  >
                    <span
                      class="activity-status"
                      :title="
                        formatEventTimestamp(
                          activityEventTimestamp(event.activity)
                        )
                      "
                      :class="{
                        'text-emerald-400':
                          event.activity.status === 'completed',
                        'text-red-400': event.activity.status === 'failed'
                      }"
                    >
                      <LoaderCircle
                        v-if="event.activity.status === 'running'"
                        class="size-3 animate-spin"
                        aria-hidden="true"
                      />
                      <Check
                        v-else-if="event.activity.status === 'completed'"
                        class="size-3"
                        aria-hidden="true"
                      />
                      <X v-else class="size-3" aria-hidden="true" />
                    </span>
                    <span class="activity-title">{{
                      event.activity.title
                    }}</span>
                    <span
                      :ref="
                        (element) => bindActivityDetail(event.key, element)
                      "
                      class="activity-detail min-w-0 flex-1 font-mono text-[11px]"
                      :class="
                        expandedActivityKeys.has(event.key)
                          ? 'whitespace-pre-wrap break-all'
                          : 'truncate'
                      "
                    >
                      {{ event.activity.detail }}
                    </span>
                    <button
                      v-if="
                        overflowingActivityKeys.has(event.key) ||
                        expandedActivityKeys.has(event.key)
                      "
                      type="button"
                      class="grid size-6 shrink-0 place-items-center rounded text-zinc-600 transition hover:bg-white/5 hover:text-zinc-300"
                      :aria-expanded="expandedActivityKeys.has(event.key)"
                      :aria-label="
                        expandedActivityKeys.has(event.key)
                          ? 'Collapse tool details'
                          : 'Expand tool details'
                      "
                      :title="
                        expandedActivityKeys.has(event.key)
                          ? 'Collapse details'
                          : 'Expand details'
                      "
                      @click="toggleActivityDetail(event.key)"
                    >
                      <ChevronUp
                        v-if="expandedActivityKeys.has(event.key)"
                        class="size-3.5"
                        aria-hidden="true"
                      />
                      <ChevronDown
                        v-else
                        class="size-3.5"
                        aria-hidden="true"
                      />
                    </button>
                  </div>
                  <div
                    v-else
                    :class="{
                      'mt-3':
                        eventIndex > 0 || turnHasCollapsibleProgress(item)
                    }"
                  >
                    <div
                      class="markdown-body"
                      :title="formatEventTimestamp(event.message.created_at)"
                      v-html="renderMarkdown(event.message.content)"
                    />
                    <button
                      type="button"
                      class="ml-auto mt-1 grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                      :aria-label="
                        copiedMessageKey === event.key
                          ? 'Message copied'
                          : 'Copy message'
                      "
                      :title="
                        copiedMessageKey === event.key
                          ? 'Copied'
                          : 'Copy message'
                      "
                      @click="
                        copyMessage(event.message.content, event.key)
                      "
                    >
                      <Check
                        v-if="copiedMessageKey === event.key"
                        class="size-3 text-emerald-400"
                        aria-hidden="true"
                      />
                      <Copy v-else class="size-3.5" aria-hidden="true" />
                    </button>
                  </div>
                </template>
                <div
                  v-if="activeAssistantTurnKey === item.key"
                  class="thinking-indicator mt-2"
                >
                  <span class="thinking-dot" />
                  <span class="thinking-dot [animation-delay:120ms]" />
                  <span class="thinking-dot [animation-delay:240ms]" />
                </div>
              </div>
            </article>
          </div>

          <div
            :style="{ height: `${virtualWindow.bottom}px` }"
            aria-hidden="true"
          />

          <div v-if="sending && !activeAssistantTurnKey" class="message-row">
            <div class="assistant-badge grid size-7 shrink-0 place-items-center rounded-md text-[10px] font-bold text-coral">
              C
            </div>
            <div class="thinking-indicator">
              <span class="thinking-dot" />
              <span class="thinking-dot [animation-delay:120ms]" />
              <span class="thinking-dot [animation-delay:240ms]" />
            </div>
          </div>
        </div>
      </div>

        <aside
          v-if="branchesOpen"
          class="branch-navigator flex w-28 shrink-0 flex-col sm:w-36 lg:w-44"
          aria-label="Conversation branches"
        >
          <header class="flex h-10 shrink-0 items-center gap-1 px-2">
            <GitBranch class="size-3.5 text-zinc-600" aria-hidden="true" />
            <span class="min-w-0 flex-1 truncate text-[9px] font-semibold uppercase tracking-[0.14em] text-zinc-600">
              Branches
            </span>
            <LoaderCircle
              v-if="branchSelecting"
              class="size-3 animate-spin text-coral"
              aria-hidden="true"
            />
            <button
              class="grid size-6 place-items-center rounded text-zinc-600 transition hover:bg-white/5 hover:text-coral disabled:opacity-30"
              :disabled="
                branchSelecting ||
                sending ||
                Boolean(editingMessageNode) ||
                !selectedBranchNode
              "
              title="Use previewed branch"
              aria-label="Use previewed branch"
              @click="confirmBranchSelection"
            >
              <Check class="size-3.5" aria-hidden="true" />
            </button>
            <button
              class="grid size-6 place-items-center rounded text-zinc-600 transition hover:bg-white/5 hover:text-zinc-300 disabled:opacity-30"
              :disabled="
                branchSelecting || sending || Boolean(editingMessageNode)
              "
              title="Cancel branch preview"
              aria-label="Cancel branch preview"
              @click="cancelBranchNavigator"
            >
              <X class="size-3.5" aria-hidden="true" />
            </button>
          </header>
          <div class="min-h-0 flex-1 overflow-auto">
            <div
              class="relative min-h-full min-w-full"
              :style="{
                width: `${Math.max(branchLayout.width, 112)}px`,
                height: `${branchLayout.height}px`
              }"
            >
              <svg
                class="pointer-events-none absolute inset-0 overflow-visible"
                :width="Math.max(branchLayout.width, 112)"
                :height="branchLayout.height"
                aria-hidden="true"
              >
                <path
                  v-for="edge in branchLayout.edges"
                  :key="`${edge.parent.id}-${edge.child.id}`"
                  :d="branchEdgePath(edge)"
                  class="branch-edge"
                  :class="branchEdgeClasses(edge)"
                />
              </svg>
              <button
                v-for="(node, index) in branchLayout.nodes"
                :key="node.id"
                type="button"
                class="branch-node"
                :class="branchNodeClasses(node)"
                :style="{
                  left: `${node.x}px`,
                  top: `${node.y}px`
                }"
                :disabled="
                  branchSelecting || sending || Boolean(editingMessageNode)
                "
                :aria-label="`Preview conversation from message ${index + 1}`"
                :aria-current="selectedBranchNode === node.id ? 'true' : undefined"
                :title="`Preview from message ${index + 1}`"
                @click="previewBranch(node.id)"
                @mouseenter="hoverBranchNode(node.id)"
                @mouseleave="leaveBranchNode(node.id)"
                @focus="hoverBranchNode(node.id)"
                @blur="leaveBranchNode(node.id)"
              />
            </div>
          </div>
          <p class="shrink-0 px-2 py-2 text-[8px] leading-3 text-zinc-700">
            Hover reveals its message; click previews its branch. You can edit
            visible messages directly. ✓ keeps it; Esc restores the original.
          </p>
        </aside>
      </div>

      <div class="composer-dock shrink-0 bg-gradient-to-t from-ink via-ink to-transparent px-3 pb-4 pt-5 sm:px-6">
        <div class="mx-auto max-w-3xl">
          <div
            v-if="error"
            class="mb-2 rounded-md border border-red-500/20 bg-red-500/8 px-3 py-2 text-xs text-red-300"
          >
            {{ error }}
          </div>
          <div v-if="session" class="relative">
            <div
              v-if="queuedPrompts.length"
              class="mb-2 max-h-48 space-y-1.5 overflow-y-auto"
            >
              <div
                v-for="(item, index) in queuedPrompts"
                :key="item.id"
                class="flex items-center gap-2 rounded-lg border px-3 py-2"
                :class="
                  selectedRuntime.queuedPromptEdit?.id === item.id
                    ? 'border-coral/25 bg-coral/6'
                    : 'border-white/8 bg-white/3'
                "
              >
                <span class="shrink-0 text-[9px] font-semibold uppercase tracking-[0.16em] text-zinc-600">
                  {{ selectedRuntime.queuedPromptEdit?.id === item.id ? "Editing" : `Queued ${index + 1}` }}
                </span>
                <span class="min-w-0 flex-1 truncate text-xs text-zinc-300">
                  {{ item.content }}
                </span>
                <button
                  type="button"
                  class="shrink-0 rounded-md border border-coral/25 bg-coral/8 px-2 py-1 text-[10px] font-semibold text-coral transition hover:bg-coral/15 disabled:cursor-not-allowed disabled:opacity-40"
                  :disabled="
                    cancelling ||
                    selectedRuntime.queuedPromptEdit?.id === item.id
                  "
                  title="Stop the current turn and send this message next"
                  @click="steerQueuedPrompt(item)"
                >
                  Steer
                </button>
                <button
                  type="button"
                  class="grid size-6 shrink-0 place-items-center rounded text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                  title="Edit queued message"
                  aria-label="Edit queued message"
                  @click="beginQueuedPromptEdit(item)"
                >
                  <Pencil class="size-3.5" aria-hidden="true" />
                </button>
                <button
                  type="button"
                  class="grid size-6 shrink-0 place-items-center rounded text-zinc-500 transition hover:bg-red-500/10 hover:text-red-300"
                  title="Delete queued message"
                  aria-label="Delete queued message"
                  @click="deleteQueuedPrompt(item)"
                >
                  <Trash2 class="size-3.5" aria-hidden="true" />
                </button>
              </div>
            </div>
            <div
              v-if="visibleGoal"
              class="mb-2 flex items-center gap-2 rounded-lg border border-violet-400/18 bg-violet-400/5 px-3 py-2"
            >
              <span
                class="shrink-0 font-mono text-[9px] font-semibold uppercase tracking-[0.14em]"
                :class="{
                  'text-violet-300': visibleGoal.status === 'active',
                  'text-amber-300': visibleGoal.status === 'paused',
                  'text-emerald-300': visibleGoal.status === 'completed',
                  'text-red-300': visibleGoal.status === 'blocked'
                }"
              >
                {{ goalStatusLabel(visibleGoal.status) }}
              </span>
              <button
                class="min-w-0 flex-1 truncate text-left text-xs text-zinc-300 hover:text-white"
                title="Show complete goal"
                @click="describedGoal = visibleGoal"
              >
                {{ compactGoal(visibleGoal, 120) }}
              </button>
              <button
                class="grid size-6 shrink-0 place-items-center rounded text-zinc-500 hover:bg-violet-400/10 hover:text-violet-300"
                :aria-label="visibleGoal.status === 'active' ? 'Pause goal' : 'Activate goal'"
                :title="visibleGoal.status === 'active' ? 'Pause goal' : 'Activate goal'"
                @click="toggleGoal(visibleGoal)"
              >
                <Pause
                  v-if="visibleGoal.status === 'active'"
                  class="size-3"
                  aria-hidden="true"
                />
                <Play v-else class="size-3" aria-hidden="true" />
              </button>
              <button
                class="grid size-6 shrink-0 place-items-center rounded text-zinc-600 hover:bg-white/5 hover:text-zinc-200"
                aria-label="Edit goal"
                title="Edit goal"
                @click="beginGoalEdit(visibleGoal)"
              >
                <Pencil class="size-3" aria-hidden="true" />
              </button>
              <button
                class="grid size-6 shrink-0 place-items-center rounded text-zinc-600 hover:bg-red-500/10 hover:text-red-300"
                aria-label="Delete goal"
                title="Delete goal"
                @click="deleteGoal(visibleGoal)"
              >
                <Trash2 class="size-3" aria-hidden="true" />
              </button>
              <button
                class="flex h-6 shrink-0 items-center gap-1 rounded px-1.5 font-mono text-[9px] text-violet-300 hover:bg-violet-400/10"
                title="Show all goals"
                @click="goalsOpen = true"
              >
                <ListTodo class="size-3" aria-hidden="true" />
                {{ goals.length }}
              </button>
            </div>
            <div
              v-if="autocomplete?.items?.length"
              class="absolute inset-x-0 bottom-full z-20 mb-2 max-h-64 overflow-y-auto rounded-lg border border-white/10 bg-[#15171a] p-1.5 shadow-2xl shadow-black/50"
              role="listbox"
              aria-label="Autocomplete suggestions"
            >
              <button
                v-for="(item, index) in autocomplete.items"
                :id="`completion-${index}`"
                :key="item.id"
                type="button"
                role="option"
                :aria-selected="index === autocompleteSelection"
                class="flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left transition"
                :class="[
                  index === autocompleteSelection
                    ? 'bg-white/8 text-white'
                    : 'text-zinc-400 hover:bg-white/5 hover:text-zinc-200',
                  item.kind === 'directory' ? 'text-amber-300' : ''
                ]"
                @mouseenter="autocompleteSelection = index"
                @mousedown.prevent="acceptAutocomplete(index)"
              >
                <span
                  v-if="item.icon"
                  class="w-5 shrink-0 text-center font-mono text-sm"
                >
                  {{ item.icon }}
                </span>
                <span
                  class="min-w-0 truncate font-mono text-xs"
                  :class="{ 'font-semibold': index === autocompleteSelection }"
                >
                  {{ completionLabel(item) }}
                </span>
                <template v-if="item.kind === 'command' || item.kind === 'skill'">
                  <span
                    class="shrink-0 rounded px-1.5 py-0.5 font-mono text-[8px] uppercase tracking-wider"
                    :class="
                      item.kind === 'skill'
                        ? 'bg-coral/10 text-coral'
                        : 'bg-cyan-400/8 text-cyan-300'
                    "
                  >
                    {{ item.kind }}
                  </span>
                  <span class="min-w-0 flex-1 truncate text-[10px] text-zinc-600">
                    {{ item.description }}
                  </span>
                </template>
              </button>
            </div>

            <div class="composer-shell">
              <textarea
                ref="composer"
                v-model="draft"
                rows="1"
                class="max-h-48 min-h-12 w-full resize-none bg-transparent px-4 py-3.5 text-sm leading-6 text-zinc-100 outline-none placeholder:text-zinc-500 disabled:cursor-not-allowed disabled:opacity-40"
                :disabled="branchesOpen || Boolean(editingMessageNode)"
                placeholder="Message CodeCrab…"
                aria-label="Message CodeCrab"
                :aria-activedescendant="
                  autocomplete?.items?.length
                    ? `completion-${autocompleteSelection}`
                    : undefined
                "
                :aria-expanded="Boolean(autocomplete?.items?.length)"
                aria-autocomplete="list"
                @input="handleComposerInput"
                @paste="pasteAttachments"
                @dragover.prevent
                @drop.prevent="dropAttachments"
                @select="refreshAutocomplete($event.target)"
                @blur="handleComposerBlur"
                @keydown="handleComposerKey"
              />
              <div class="composer-toolbar flex items-center gap-2 px-2.5 pb-2.5">
                <div
                  v-if="!recording"
                  class="composer-model-controls flex min-w-0 items-center gap-1.5"
                  aria-label="Model configuration"
                >
                  <select
                    v-if="session"
                    class="control composer-control composer-model-control"
                    :value="session.model"
                    :disabled="branchesOpen || Boolean(editingMessageNode)"
                    aria-label="Model"
                    title="Model"
                    @change="chooseModel"
                  >
                    <option v-for="model in models" :key="model.slug" :value="model.slug">
                      {{ model.display_name }}
                    </option>
                  </select>
                  <select
                    v-if="reasoningOptions.length"
                    class="control composer-control composer-param-control"
                    :value="session?.reasoning_effort ?? ''"
                    :disabled="branchesOpen || Boolean(editingMessageNode)"
                    aria-label="Thinking level"
                    title="Thinking level"
                    @change="updateModel({ reasoning_effort: $event.target.value || null })"
                  >
                    <option
                      v-for="option in reasoningOptions"
                      :key="option.effort"
                      :value="option.effort"
                    >
                      {{ option.name }}
                    </option>
                  </select>
                  <select
                    v-if="speedOptions.length"
                    class="control composer-control composer-param-control"
                    :value="session?.service_tier ?? ''"
                    :disabled="branchesOpen || Boolean(editingMessageNode)"
                    aria-label="Service speed"
                    title="Service speed"
                    @change="updateModel({ service_tier: $event.target.value || null })"
                  >
                    <option value="">standard</option>
                    <option v-for="tier in speedOptions" :key="tier.id" :value="tier.id">
                      {{ tier.name.toLowerCase() }}
                    </option>
                  </select>
                </div>
                <canvas
                  v-show="recording"
                  ref="waveformCanvas"
                  class="h-7 min-w-0 flex-1"
                  aria-hidden="true"
                />
                <div class="ml-auto flex items-center gap-1">
                  <input
                    ref="attachmentInput"
                    type="file"
                    class="hidden"
                    multiple
                    @change="selectAttachments"
                  />
                  <button
                    class="grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200 disabled:text-zinc-700"
                    :disabled="
                      branchesOpen ||
                      Boolean(editingMessageNode) ||
                      uploadingAttachments
                    "
                    title="Attach files"
                    aria-label="Attach files"
                    @click="attachmentInput?.click()"
                  >
                    <LoaderCircle
                      v-if="uploadingAttachments"
                      class="size-3.5 animate-spin"
                      aria-hidden="true"
                    />
                    <Paperclip v-else class="size-3.5" aria-hidden="true" />
                  </button>
                  <button
                    v-if="recording"
                    class="grid size-7 place-items-center rounded-md text-zinc-500 transition hover:bg-white/5 hover:text-zinc-200"
                    title="Discard recording"
                    aria-label="Discard recording"
                    @click="cancelDictation"
                  >
                    <X class="size-3.5" aria-hidden="true" />
                  </button>
                  <button
                    class="grid size-7 place-items-center rounded-md text-sm transition disabled:cursor-not-allowed disabled:text-zinc-700"
                    :class="
                      recording
                        ? 'animate-pulse bg-red-500/15 text-red-400 hover:bg-red-500/25'
                        : 'text-zinc-500 hover:bg-white/5 hover:text-zinc-200'
                    "
                    :disabled="
                      branchesOpen ||
                      Boolean(editingMessageNode) ||
                      transcribing ||
                      !dictationAvailable
                    "
                    :title="
                      dictationAvailable
                        ? recording
                          ? 'Stop and transcribe dictation'
                          : 'Start voice dictation'
                        : 'Dictation requires the official OpenAI provider and valid authentication'
                    "
                    :aria-label="recording ? 'Stop and transcribe dictation' : 'Start voice dictation'"
                    @click="toggleDictation"
                  >
                    <LoaderCircle
                      v-if="transcribing"
                      class="size-3.5 animate-spin"
                      aria-hidden="true"
                    />
                    <Mic v-else class="size-3.5" aria-hidden="true" />
                  </button>
                  <button
                    class="grid size-7 place-items-center rounded-md bg-zinc-100 text-sm text-zinc-950 transition hover:bg-white disabled:cursor-not-allowed disabled:bg-zinc-800 disabled:text-zinc-600"
                    :disabled="
                      branchesOpen || Boolean(editingMessageNode)
                        ? true
                        : recording
                        ? false
                        : sending
                        ? cancelling
                        : transcribing || !draft.trim()
                    "
                    :aria-label="
                      recording
                        ? 'Stop dictation and send'
                        : sending
                        ? 'Stop agent'
                        : 'Send message'
                    "
                    :title="
                      recording
                        ? 'Stop dictation and send'
                        : sending
                        ? 'Stop agent'
                        : 'Send message'
                    "
                    @click="
                      recording
                        ? sendPrompt()
                        : sending
                          ? cancelTurn()
                          : sendPrompt()
                    "
                  >
                    <Square
                      v-if="sending && !recording"
                      class="size-3 fill-current"
                      aria-hidden="true"
                    />
                    <ArrowUp v-else class="size-3.5" aria-hidden="true" />
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </main>
  </div>
</template>
