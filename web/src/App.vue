<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import {
  ArrowUp,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronUp,
  Copy,
  Folder,
  ListTodo,
  LoaderCircle,
  Menu,
  Mic,
  Settings,
  Pause,
  Pencil,
  Play,
  Plus,
  Square,
  Trash2,
  X
} from "@lucide/vue";
import { renderMarkdown } from "./markdown.js";
import { isScrolledToBottom } from "./scroll.js";
import {
  activityEventTimestamp,
  formatEventTimestamp
} from "./timestamps.js";
import { sortChronologically } from "./timeline.js";

const state = ref(null);
const draft = ref("");
const loading = ref(true);
const sending = ref(false);
const cancelling = ref(false);
const queuedPrompt = ref("");
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
const sidebarView = ref("sessions");
const selectedProjectRoot = ref("");
const copiedMessageKey = ref("");
const expandedActivityKeys = ref(new Set());
const overflowingActivityKeys = ref(new Set());
const expandedTurnKeys = ref(new Set());
const goalsOpen = ref(false);
const providersOpen = ref(false);
const providerDraft = ref(null);
const describedGoal = ref(null);
const editingGoal = ref(null);
const goalDraft = ref("");
const editingGoalResume = ref(false);
let autocompleteRequest = 0;
let copiedMessageTimer = null;
const activityDetailElements = new Map();
const activityDetailObservers = new Map();
let mediaRecorder = null;
let microphoneStream = null;
let recordingChunks = [];
let discardRecording = false;
let sendAfterTranscription = false;
let audioContext = null;
let microphoneSource = null;
let audioAnalyser = null;
let waveformFrame = null;
let waveformData = null;
let lastEscapeAt = 0;
let pendingGoalAction = null;

const session = computed(() => state.value?.session ?? null);
const projects = computed(() => state.value?.projects ?? []);
const selectedProject = computed(
  () =>
    projects.value.find(
      (project) => project.root === selectedProjectRoot.value
    ) ??
    projects.value[0] ??
    null
);
const models = computed(() => state.value?.models ?? []);
const dictationAvailable = computed(
  () => state.value?.dictation_available ?? false
);
const providers = computed(() => state.value?.providers ?? []);
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

function assistantTurnEvents(messages, activities, turnMessageIndex) {
  const events = [];
  const matchedActivities = new Set();

  for (const [messageIndex, message] of messages.entries()) {
    if (message.role !== "assistant") continue;
    if (!message.hidden && message.content?.trim()) {
      events.push({
        type: "message",
        key: `message-${turnMessageIndex}-${messageIndex}`,
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
          key: `activity-${activity.id}`,
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
        key: `activity-${activity.id}`,
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
              key: `message-orphan-${messageIndex}`,
              message
            }
          ])
        );
      }
      messageIndex += 1;
      continue;
    }

    if (!message.hidden && message.content?.trim()) {
      items.push({
        type: "message",
        key: `message-${messageIndex}`,
        message
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
      messageIndex
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
const speedOptions = computed(() => selectedModel.value?.service_tiers ?? []);

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
    if (
      resumeGoal &&
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

async function newSession() {
  await runAction(
    () => api("/api/sessions", { method: "POST", body: "{}" }),
    {
      selectActiveProject: true,
      closeSidebar: true,
      historyMode: "push"
    }
  );
}

async function resumeSession(project, id) {
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
  if (nextState?.session?.goals?.some((goal) => goal.status === "active")) {
    void runPrompt("", { continuation: true });
  }
}

async function deleteSession(project, id) {
  if (sending.value) return;
  const deletingActive =
    project === state.value?.project && id === session.value?.id;
  error.value = "";
  try {
    const nextState = await api("/api/sessions/delete", {
      method: "POST",
      body: JSON.stringify({ project, id })
    });
    applyServerState(nextState, {
      selectActiveProject: deletingActive
    });
    if (deletingActive) {
      updateSessionUrl(nextState.session?.id, "replace");
    } else if (
      !nextState.projects.some(
        (candidate) => candidate.root === selectedProjectRoot.value
      )
    ) {
      sidebarView.value = "projects";
    }
  } catch (cause) {
    error.value = cause.message;
  }
}

function applyServerState(nextState, { selectActiveProject = false } = {}) {
  if (nextState.session?.id !== session.value?.id) {
    autoScroll.value = true;
  }
  state.value = nextState;
  if (selectActiveProject || !selectedProjectRoot.value) {
    selectedProjectRoot.value = nextState.project;
    sidebarView.value = "sessions";
  }
}

function selectProject(root) {
  selectedProjectRoot.value = root;
  sidebarView.value = "sessions";
}

function showProjects() {
  sidebarView.value = "projects";
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
  error.value = "";
  try {
    const nextState = await api("/api/sessions/resume", {
      method: "POST",
      body: JSON.stringify({ id })
    });
    applyServerState(nextState, { selectActiveProject: true });
    await scrollToBottom();
    if (activeGoal.value) void runPrompt("", { continuation: true });
  } catch (cause) {
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

async function clearSession() {
  await runAction(() => api("/api/session/clear", { method: "POST", body: "{}" }));
}

function applyGoalState(nextState) {
  applyServerState(nextState);
}

async function goalRequest(path, { method = "POST", id, objective } = {}) {
  const nextState = await api(`/api/goals/${path}`, {
    method,
    body: JSON.stringify({ id, objective })
  });
  applyGoalState(nextState);
  return nextState;
}

async function afterCurrentTurn(action) {
  const guarded = async () => {
    error.value = "";
    try {
      await action();
    } catch (cause) {
      error.value = cause.message;
    }
  };
  if (sending.value) {
    pendingGoalAction = guarded;
    await cancelTurn({ pauseGoal: false });
    return;
  }
  await guarded();
}

async function createGoal(objective) {
  await afterCurrentTurn(async () => {
    await goalRequest("create", { objective });
    void runPrompt(objective);
  });
}

async function toggleGoal(goal) {
  await afterCurrentTurn(async () => {
    if (goal.status === "active") {
      await goalRequest("pause", { id: goal.id });
    } else {
      await goalRequest("activate", { id: goal.id });
      goalsOpen.value = false;
      void runPrompt("", { continuation: true });
    }
  });
}

async function deleteGoal(goal) {
  await afterCurrentTurn(async () => {
    await goalRequest("delete", { id: goal.id });
    if (describedGoal.value?.id === goal.id) describedGoal.value = null;
    if (editingGoal.value?.id === goal.id) editingGoal.value = null;
  });
}

async function beginGoalEdit(goal) {
  const resume = goal.status === "active";
  await afterCurrentTurn(async () => {
    if (resume) await goalRequest("pause", { id: goal.id });
    editingGoal.value = { ...goal };
    goalDraft.value = goal.objective;
    editingGoalResume.value = resume;
  });
}

async function saveGoalEdit() {
  const goal = editingGoal.value;
  const objective = goalDraft.value.trim();
  if (!goal || !objective) return;
  try {
    await goalRequest("edit", {
      method: "PUT",
      id: goal.id,
      objective
    });
    const resume = editingGoalResume.value;
    editingGoal.value = null;
    editingGoalResume.value = false;
    if (resume) {
      await goalRequest("activate", { id: goal.id });
      void runPrompt("", { continuation: true });
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
      await goalRequest("activate", { id: goal.id });
      void runPrompt("", { continuation: true });
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
  const transcript = text.trim();
  if (!transcript) return false;
  const element = composer.value;
  const start = element?.selectionStart ?? draft.value.length;
  const end = element?.selectionEnd ?? start;
  const before = draft.value.slice(0, start);
  const after = draft.value.slice(end);
  const leading = before && !/\s$/.test(before) ? " " : "";
  const trailing = after && !/^\s/.test(after) ? " " : "";
  const insertion = `${leading}${transcript}${trailing}`;
  draft.value = before + insertion + after;
  const cursor = start + insertion.length;
  await nextTick();
  element?.focus();
  element?.setSelectionRange(cursor, cursor);
  resizeComposer();
  await refreshAutocomplete(element);
  return true;
}

async function transcribeRecording() {
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
      headers: { "Content-Type": contentType },
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

async function toggleDictation() {
  if (recording.value) {
    sendAfterTranscription = false;
    mediaRecorder?.stop();
    return;
  }
  if (transcribing.value || !dictationAvailable.value) return;
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
  if (!session.value) return;
  await runAction(() =>
    api("/api/model", {
      method: "PUT",
      body: JSON.stringify({
        model: patch.model ?? session.value.model,
        reasoning_effort:
          patch.reasoning_effort === undefined
            ? session.value.reasoning_effort
            : patch.reasoning_effort,
        service_tier:
          patch.service_tier === undefined
            ? session.value.service_tier
            : patch.service_tier
      })
    })
  );
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

function applyActivity(activity) {
  const current = session.value?.activities ?? [];
  const index = current.findIndex((item) => item.id === activity.id);
  const activities =
    index === -1
      ? [...current, activity]
      : current.map((item, itemIndex) =>
          itemIndex === index ? activity : item
        );
  state.value = {
    ...state.value,
    session: {
      ...session.value,
      activities
    }
  };
}

function applyAssistantMessage(message) {
  state.value = {
    ...state.value,
    session: {
      ...session.value,
      messages: [...(session.value?.messages ?? []), message]
    }
  };
}

function applyAssistantTextDelta(delta, start, sequence, createdAt) {
  const messages = [...(session.value?.messages ?? [])];
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
  state.value = {
    ...state.value,
    session: {
      ...session.value,
      messages
    }
  };
}

function applyAssistantStreamReset() {
  const messages = [...(session.value?.messages ?? [])];
  if (messages.at(-1)?.role === "assistant") {
    messages.pop();
  }
  state.value = {
    ...state.value,
    session: {
      ...session.value,
      messages
    }
  };
}

function applyAssistantMessageCompleted(message) {
  const messages = [...(session.value?.messages ?? [])];
  const index = messages.findLastIndex((item) => item.role === "assistant");
  if (index === -1) messages.push(message);
  else messages[index] = message;
  state.value = {
    ...state.value,
    session: {
      ...session.value,
      messages
    }
  };
}

async function handleChatStreamEvent(event) {
  if (event.type === "user_message") {
    applyAssistantMessage(event.message);
    await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_message") {
    applyAssistantMessage(event.message);
    await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_text_delta") {
    applyAssistantTextDelta(
      event.delta,
      event.start,
      event.sequence,
      event.created_at
    );
    await scrollToBottom();
    return false;
  }
  if (event.type === "assistant_stream_reset") {
    applyAssistantStreamReset();
    return false;
  }
  if (event.type === "assistant_message_completed") {
    applyAssistantMessageCompleted(event.message);
    return false;
  }
  if (event.type === "activity") {
    applyActivity(event.activity);
    await scrollToBottom();
    return false;
  }
  if (event.type === "done") {
    state.value = event.state;
    return true;
  }
  if (event.type === "cancelled") {
    state.value = event.state;
    return true;
  }
  if (event.type === "error") {
    throw new Error(event.error || "The agent turn failed");
  }
  return false;
}

async function streamChat(prompt, { continuation = false } = {}) {
  const response = await fetch("/api/chat", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ prompt, continuation })
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
        completed = (await handleChatStreamEvent(JSON.parse(line))) || completed;
      }
    }
    if (done) break;
  }
  if (buffer.trim()) {
    completed =
      (await handleChatStreamEvent(JSON.parse(buffer))) || completed;
  }
  if (!completed) {
    throw new Error("The agent stream ended before returning its final state");
  }
}

async function sendPrompt() {
  if (recording.value) {
    sendAfterTranscription = true;
    mediaRecorder?.stop();
    return;
  }
  if (transcribing.value) return;
  const prompt = draft.value.trim();
  if (!prompt) return;
  if (prompt === "/goals") {
    await clearComposer();
    closeAutocomplete();
    goalsOpen.value = true;
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
    if (!queuedPrompt.value) {
      queuedPrompt.value = prompt;
      await clearComposer();
      closeAutocomplete();
    }
    return;
  }
  await clearComposer();
  await runPrompt(prompt);
}

async function runPrompt(prompt, { continuation = false } = {}) {
  closeAutocomplete();
  sending.value = true;
  cancelling.value = false;
  error.value = "";

  await scrollToBottom();

  let completed = false;
  try {
    await streamChat(prompt, { continuation });
    completed = true;
  } catch (cause) {
    const streamError = cause.message;
    await loadState();
    error.value = streamError;
  } finally {
    sending.value = false;
    cancelling.value = false;
    await scrollToBottom();
    const goalAction = pendingGoalAction;
    pendingGoalAction = null;
    if (goalAction) {
      void goalAction();
      return;
    }
    const nextPrompt = queuedPrompt.value;
    queuedPrompt.value = "";
    if (nextPrompt) {
      void runPrompt(nextPrompt);
    } else if (completed && activeGoal.value) {
      void runPrompt("", { continuation: true });
    } else {
      composer.value?.focus();
    }
  }
}

async function cancelTurn({ pauseGoal = true } = {}) {
  if (!sending.value || cancelling.value) return;
  if (pauseGoal && activeGoal.value && !pendingGoalAction) {
    const id = activeGoal.value.id;
    pendingGoalAction = () => goalRequest("pause", { id });
  }
  cancelling.value = true;
  error.value = "";
  try {
    await api("/api/chat/cancel", {
      method: "POST",
      body: "{}"
    });
  } catch (cause) {
    cancelling.value = false;
    error.value = `Could not stop the agent: ${cause.message}`;
  }
}

function steerQueuedPrompt() {
  cancelTurn({ pauseGoal: false });
}

function handleGlobalKeydown(event) {
  if (event.repeat) return;
  if (event.key !== "Escape") {
    lastEscapeAt = 0;
    return;
  }
  if (editingGoal.value) {
    event.preventDefault();
    void cancelGoalEdit();
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
  autocompleteRequest += 1;
  autocomplete.value = null;
  autocompleteSelection.value = 0;
}

async function refreshAutocomplete(element = composer.value) {
  if (!element || sending.value) {
    closeAutocomplete();
    return;
  }
  const cursor = element.selectionStart ?? draft.value.length;
  const request = ++autocompleteRequest;
  try {
    const result = await api("/api/completions", {
      method: "POST",
      body: JSON.stringify({
        before_cursor: draft.value.slice(0, cursor),
        after_cursor: draft.value.slice(cursor)
      })
    });
    if (request !== autocompleteRequest) return;
    autocomplete.value = result;
    autocompleteSelection.value = Math.min(
      autocompleteSelection.value,
      Math.max((result?.items?.length ?? 1) - 1, 0)
    );
  } catch {
    if (request === autocompleteRequest) {
      autocomplete.value = null;
      autocompleteSelection.value = 0;
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
  return (
    item.name
      .replaceAll("\\", "/")
      .split("/")
      .filter(Boolean)
      .at(-1) ?? item.name
  );
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

async function clearComposer() {
  draft.value = "";
  await nextTick();
  resizeComposer();
}

function handleConversationScroll() {
  if (conversation.value) {
    autoScroll.value = isScrolledToBottom(conversation.value);
  }
}

async function scrollToBottom() {
  if (!autoScroll.value) return;
  await nextTick();
  if (autoScroll.value && conversation.value) {
    conversation.value.scrollTop = conversation.value.scrollHeight;
  }
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

function formatTime(value) {
  if (!value) return "";
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit"
  }).format(new Date(value));
}

onMounted(() => {
  window.addEventListener("popstate", handleHistoryNavigation);
  window.addEventListener("keydown", handleGlobalKeydown);
  loadState({ resumeGoal: true });
});
onBeforeUnmount(() => {
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
  <div class="h-dvh overflow-hidden bg-ink text-zinc-200">
    <div
      v-if="sidebarOpen"
      class="fixed inset-0 z-30 bg-black/60 backdrop-blur-sm lg:hidden"
      @click="sidebarOpen = false"
    />

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
            <p class="mt-0.5 text-[10px] text-zinc-600">API keys are saved in the platform configuration file.</p>
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

    <aside
      class="fixed inset-y-0 left-0 z-40 flex w-72 -translate-x-full flex-col border-r border-white/6 bg-panel transition-transform duration-200 lg:translate-x-0"
      :class="{ 'translate-x-0': sidebarOpen }"
    >
      <div class="flex h-14 items-center gap-3 border-b border-white/6 px-4">
        <div class="grid size-7 place-items-center rounded-md bg-coral text-sm font-black text-black">
          C
        </div>
        <span class="text-sm font-semibold tracking-tight text-white">CodeCrab</span>
      </div>

      <div class="space-y-2 p-3">
        <button
          v-if="sidebarView === 'sessions' && selectedProject"
          class="flex w-full items-center gap-2 rounded-md px-2 py-2 text-left text-cyan-300 transition hover:bg-white/4 hover:text-cyan-200"
          :title="selectedProject.root"
          aria-label="Show projects"
          @click="showProjects"
        >
          <ChevronLeft class="size-4 shrink-0" aria-hidden="true" />
          <Folder class="size-3.5 shrink-0" aria-hidden="true" />
          <span class="min-w-0 flex-1 truncate text-xs font-semibold">
            {{ projectName(selectedProject.root) }}
          </span>
          <span class="font-mono text-[9px] text-zinc-700">
            {{ selectedProject.sessions.length }}
          </span>
        </button>
        <button
          class="flex w-full items-center justify-center gap-2 rounded-md border border-white/8 bg-white/4 px-3 py-2 text-xs font-medium text-zinc-200 transition hover:border-white/15 hover:bg-white/7"
          @click="newSession"
        >
          <Plus class="size-3.5 text-coral" aria-hidden="true" />
          New session
        </button>
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-4">
        <template v-if="sidebarView === 'sessions' && selectedProject">
          <p class="px-2 pb-2 pt-1 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-600">
            Sessions
          </p>
          <p
            v-if="!selectedProject.sessions.length"
            class="px-2 py-3 text-xs leading-5 text-zinc-600"
          >
            No sessions yet. Use <span class="text-zinc-400">New session</span>
            to start one.
          </p>
          <div
            v-for="item in selectedProject.sessions"
            :key="item.id"
            class="group mb-0.5 flex w-full items-center rounded-md transition hover:bg-white/4"
            :class="{
              'bg-white/5': isCurrentSession(selectedProject, item)
            }"
          >
            <button
              class="min-w-0 flex-1 px-2.5 py-2 text-left"
              @click="resumeSession(selectedProject.root, item.id)"
            >
              <span class="block truncate text-xs text-zinc-300 group-hover:text-white">
                {{ item.title || "New session" }}
              </span>
              <span class="mt-1 flex items-center justify-between font-mono text-[9px] text-zinc-600">
                <span>{{ shortId(item.id) }}</span>
                <span>{{ formatTime(item.updated_at) }}</span>
              </span>
            </button>
            <button
              class="mr-1 grid size-8 shrink-0 place-items-center rounded text-sm text-zinc-700 transition hover:bg-red-500/10 hover:text-red-400 disabled:pointer-events-none disabled:opacity-30"
              :disabled="sending"
              :aria-label="`Delete session ${item.title || shortId(item.id)}`"
              title="Delete session"
              @click.stop="deleteSession(selectedProject.root, item.id)"
            >
              <Trash2 class="size-3.5" aria-hidden="true" />
            </button>
          </div>
        </template>

        <template v-else>
          <p class="px-2 pb-2 pt-1 text-[10px] font-semibold uppercase tracking-[0.16em] text-zinc-600">
            Projects
          </p>
          <button
            v-for="project in projects"
            :key="project.root"
            class="mb-0.5 flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left transition hover:bg-white/4"
            :class="{
              'bg-white/5': project.root === selectedProjectRoot
            }"
            :title="project.root"
            @click="selectProject(project.root)"
          >
            <Folder class="size-3.5 shrink-0 text-cyan-400" aria-hidden="true" />
            <span class="min-w-0 flex-1">
              <span class="block truncate text-xs font-semibold text-cyan-300">
                {{ projectName(project.root) }}
              </span>
              <span class="mt-1 block truncate font-mono text-[9px] text-zinc-700">
                {{ project.root }}
              </span>
            </span>
            <span
              v-if="project.root === state?.project"
              class="rounded bg-cyan-400/8 px-1.5 py-0.5 font-mono text-[8px] uppercase tracking-wider text-cyan-400"
            >
              current
            </span>
            <span class="font-mono text-[9px] text-zinc-700">
              {{ project.sessions.length }}
            </span>
          </button>
        </template>
      </div>
    </aside>

    <main class="flex h-full min-w-0 flex-col lg:pl-72">
      <header class="flex h-14 shrink-0 items-center gap-3 border-b border-white/6 bg-ink/90 px-4 backdrop-blur">
        <button
          class="grid size-8 place-items-center rounded-md text-zinc-500 hover:bg-white/5 hover:text-white lg:hidden"
          aria-label="Open sessions"
          @click="sidebarOpen = true"
        >
          <Menu class="size-4" aria-hidden="true" />
        </button>

        <div class="min-w-0">
          <h1 class="truncate text-xs font-medium text-zinc-200">
            {{ session?.title || "No session" }}
          </h1>
          <p class="mt-0.5 truncate font-mono text-[9px] text-zinc-600">
            {{ state?.project }}
          </p>
        </div>

        <div class="ml-auto flex items-center gap-2">
          <button class="grid size-8 place-items-center rounded-md text-zinc-600 transition hover:bg-white/5 hover:text-zinc-300" title="Providers" aria-label="Manage providers" @click="providersOpen = true"><Settings class="size-4" /></button>
          <select
            v-if="session"
            class="control max-w-44"
            :value="session.model"
            aria-label="Model"
            @change="chooseModel"
          >
            <option v-for="model in models" :key="model.slug" :value="model.slug">
              {{ model.display_name }}
            </option>
          </select>
          <select
            v-if="reasoningOptions.length"
            class="control hidden sm:block"
            :value="session?.reasoning_effort ?? ''"
            aria-label="Thinking level"
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
            class="control hidden sm:block"
            :value="session?.service_tier ?? ''"
            aria-label="Service speed"
            @change="updateModel({ service_tier: $event.target.value || null })"
          >
            <option value="">standard</option>
            <option v-for="tier in speedOptions" :key="tier.id" :value="tier.id">
              {{ tier.name.toLowerCase() }}
            </option>
          </select>
          <button
            v-if="session"
            class="grid size-8 place-items-center rounded-md text-zinc-600 transition hover:bg-white/5 hover:text-zinc-300"
            title="Clear conversation"
            aria-label="Clear conversation"
            @click="clearSession"
          >
            <X class="size-4" aria-hidden="true" />
          </button>
        </div>
      </header>

      <div
        ref="conversation"
        class="min-h-0 flex-1 overflow-y-auto"
        @scroll.passive="handleConversationScroll"
      >
        <div v-if="loading" class="grid h-full place-items-center">
          <div class="flex items-center gap-2 text-xs text-zinc-600">
            <span class="size-1.5 animate-pulse rounded-full bg-coral" />
            Loading workspace
          </div>
        </div>

        <div v-else-if="!timeline.length" class="h-full" />

        <div v-else class="mx-auto max-w-3xl px-4 py-8 sm:px-8">
          <template v-for="item in timeline" :key="item.key">
            <article v-if="item.type === 'message'" class="message-row group">
              <div
                class="grid size-6 shrink-0 place-items-center rounded-md bg-white/7 text-[10px] font-bold text-zinc-300"
              >
                U
              </div>
              <div class="min-w-0 flex-1">
                <div
                  class="message-content"
                  :title="formatEventTimestamp(item.message.created_at)"
                >
                  {{ item.message.content }}
                </div>
                <button
                  type="button"
                  class="ml-auto mt-1 grid size-3 place-items-center text-zinc-600 transition hover:text-zinc-300"
                  :aria-label="
                    copiedMessageKey === item.key
                      ? 'Message copied'
                      : 'Copy message'
                  "
                  :title="
                    copiedMessageKey === item.key ? 'Copied' : 'Copy message'
                  "
                  @click="copyMessage(item.message.content, item.key)"
                >
                  <Check
                    v-if="copiedMessageKey === item.key"
                    class="size-3 text-emerald-400"
                    aria-hidden="true"
                  />
                  <Copy v-else class="size-3" aria-hidden="true" />
                </button>
              </div>
            </article>

            <article v-else class="message-row group">
              <div class="grid size-6 shrink-0 place-items-center rounded-md bg-coral/12 text-[10px] font-bold text-coral">
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
                      class="min-w-0 flex-1 font-mono text-[10px] text-zinc-600"
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
                      class="ml-auto mt-1 grid size-3 place-items-center text-zinc-600 transition hover:text-zinc-300"
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
                      <Copy v-else class="size-3" aria-hidden="true" />
                    </button>
                  </div>
                </template>
                <div
                  v-if="activeAssistantTurnKey === item.key"
                  class="mt-2 flex gap-1"
                >
                  <span class="thinking-dot" />
                  <span class="thinking-dot [animation-delay:120ms]" />
                  <span class="thinking-dot [animation-delay:240ms]" />
                </div>
              </div>
            </article>
          </template>

          <div v-if="sending && !activeAssistantTurnKey" class="message-row">
            <div class="mt-0.5 grid size-6 shrink-0 place-items-center rounded-md bg-coral/12 text-[10px] font-bold text-coral">
              C
            </div>
            <div class="pt-1.5">
              <div class="flex gap-1">
                <span class="thinking-dot" />
                <span class="thinking-dot [animation-delay:120ms]" />
                <span class="thinking-dot [animation-delay:240ms]" />
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="shrink-0 bg-gradient-to-t from-ink via-ink to-transparent px-3 pb-3 pt-6 sm:px-6">
        <div class="mx-auto max-w-3xl">
          <div
            v-if="error"
            class="mb-2 rounded-md border border-red-500/20 bg-red-500/8 px-3 py-2 text-xs text-red-300"
          >
            {{ error }}
          </div>
          <div v-if="session" class="relative">
            <div
              v-if="queuedPrompt"
              class="mb-2 flex items-center gap-3 rounded-lg border border-white/8 bg-white/3 px-3 py-2"
            >
              <span class="shrink-0 text-[9px] font-semibold uppercase tracking-[0.16em] text-zinc-600">
                Queued
              </span>
              <span class="min-w-0 flex-1 truncate text-xs text-zinc-300">
                {{ queuedPrompt }}
              </span>
              <button
                type="button"
                class="shrink-0 rounded-md border border-coral/25 bg-coral/8 px-2 py-1 text-[10px] font-semibold text-coral transition hover:bg-coral/15 disabled:cursor-wait disabled:opacity-50"
                :disabled="cancelling"
                title="Stop the current turn and send this message"
                @click="steerQueuedPrompt"
              >
                Steer
              </button>
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
                :key="`${item.kind}-${item.name}`"
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
                class="max-h-48 min-h-11 w-full resize-none bg-transparent px-3 py-3 text-sm leading-5 text-zinc-200 outline-none placeholder:text-zinc-700"
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
                @select="refreshAutocomplete($event.target)"
                @blur="handleComposerBlur"
                @keydown="handleComposerKey"
              />
              <div class="flex items-center gap-2 px-2 pb-2">
                <canvas
                  v-show="recording"
                  ref="waveformCanvas"
                  class="h-7 min-w-0 flex-1"
                  aria-hidden="true"
                />
                <div class="ml-auto flex items-center gap-1">
                  <button
                    class="grid size-7 place-items-center rounded-md text-sm transition disabled:cursor-not-allowed disabled:text-zinc-700"
                    :class="
                      recording
                        ? 'animate-pulse bg-red-500/15 text-red-400 hover:bg-red-500/25'
                        : 'text-zinc-500 hover:bg-white/5 hover:text-zinc-200'
                    "
                    :disabled="transcribing || !dictationAvailable"
                    :title="
                      dictationAvailable
                        ? recording
                          ? 'Stop dictation'
                          : 'Start voice dictation'
                        : 'Sign in with ChatGPT OAuth to use dictation'
                    "
                    :aria-label="recording ? 'Stop dictation' : 'Start voice dictation'"
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
                      recording
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
