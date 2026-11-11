import { classifyIntent, composeScene, validateScene, validateModelScene, bindModelScene } from '/scene.js';
import { LayoutCoordinator } from '/layout-client.js';
import { RealtimeVoiceSession } from '/realtime-voice.js';
import { VoiceToolRouter } from '/voice-tools.js';

const $ = selector => document.querySelector(selector);
const e = value => String(value ?? '').replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char]);
const icon = (name, className = '') => `<svg class="${e(className)}" aria-hidden="true"><use href="#i-${e(name)}"/></svg>`;
const storage = {
  get(key, fallback) { try { return JSON.parse(localStorage.getItem(`agentos.${key}`)) ?? fallback; } catch { return fallback; } },
  set(key, value) { try { localStorage.setItem(`agentos.${key}`, JSON.stringify(value)); return true; } catch { return false; } },
};
const initialNotes = storage.get('notes', []);
const initialLog = storage.get('activity', []);
const initialFocus = storage.get('focus', {});
const prefersReducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
const state = {
  scene: 'overview', connected: false, connection: 'starting', busy: false,
  notes: Array.isArray(initialNotes) ? initialNotes.filter(n => n && typeof n.text === 'string' && typeof n.id === 'string').slice(0, 100) : [],
  remoteNotes: [], remoteNotesLoaded: false,
  notebookTasks: new Map(), notebookReadVersion: 0, notebookRefreshNeeded: false,
  localLog: Array.isArray(initialLog) ? initialLog.filter(t => t && typeof t.id === 'string' && typeof t.text === 'string').slice(0, 40) : [],
  tasks: [], selected: null, nextBefore: null, taskHistory: [], currentBefore: null,
  focus: { duration: [25, 45, 60].includes(initialFocus.duration) ? initialFocus.duration : 25,
    remaining: Number.isFinite(initialFocus.remaining) ? Math.max(0, Math.min(3600, initialFocus.remaining)) : 1500,
    endAt: Number.isFinite(initialFocus.endAt) ? initialFocus.endAt : null,
    goal: typeof initialFocus.goal === 'string' ? initialFocus.goal.slice(0, 2000) : '' },
  speech: storage.get('speech', false) === true,
  motion: storage.get('motion', !prefersReducedMotion.matches) !== false && !prefersReducedMotion.matches,
  voiceConsent: storage.get('realtimeVoiceConsent', false) === true,
  voiceAvailable: false, voiceModel: '', voiceCaptions: [],
  voice: { phase: 'idle', active: false, muted: false, userSpeaking: false, assistantSpeaking: false },
  token: '', capabilities: [], approval: null,
  revision: null, lastIntent: '',
  layoutEnabled: storage.get('modelLayout', true) !== false,
  layoutAvailable: false, layoutStatus: 'idle', layoutModel: '', layoutError: '',
  generatedScene: null, generatedKey: '',
};
const sceneLabels = { overview: ['01', '此刻'], focus: ['02', '专注空间'], memory: ['03', '记忆'], activity: ['04', '任务轨迹'], clock: ['05', '时间'], help: ['06', '能力'], task: ['04', '意图执行'], note: ['03', '记忆'] };
const stateLabels = { queued: '等待执行', planning: '正在规划', running: '正在执行', succeeded: '已完成', failed: '执行失败', awaiting_confirmation: '等待你的批准', cancelled: '已取消', interrupted: '已中断', pending: '尚未开始' };
const terminalStates = new Set(['succeeded', 'failed', 'cancelled', 'interrupted']);
let orbFrame = 0;
let orbResizeObserver = null;
let orbRedraw = null;
let toastTimer;
let refreshInFlight = false;
let taskSelectionVersion = 0;
let sceneRequestVersion = 0;
let lastAnnouncedTask = '';
let lastAnnouncedConfirmation = '';
let canvasDirty = false;
let layoutStatusInFlight = false;
let viewportTimer;
let voiceStatusInFlight = false;
let lastVoiceContext = '';

const layoutCoordinator = new LayoutCoordinator({
  request: requestModelLayout,
  onScene(response, key) {
    if (!state.layoutEnabled || key !== layoutContext().key) return;
    state.generatedScene = response.scene;
    state.generatedKey = key;
    canvasDirty = true;
    renderWhenUnoccupied();
  },
  onStatus(status) {
    state.layoutStatus = status.state;
    state.layoutError = status.error?.message || '';
    updateLayoutIndicator();
  },
  debounceMs: 650,
});

const voiceToolRouter = new VoiceToolRouter({
  performIntent: text => submitIntent(text, { fromVoice: true }),
  showSpace: async space => {
    await navigate(space);
    return { ok: true, scene: state.scene, ...voiceConversationFacts() };
  },
  controlFocus: controlVoiceFocus,
});
const realtimeVoice = new RealtimeVoiceSession({
  createSession: createVoiceSession,
  releaseSession: async ({ callId }) => {
    if (callId && state.token) await voiceApi('/voice/hangup', { call_id: callId });
  },
  onState: updateVoiceState,
  onTranscript: updateVoiceCaption,
  onToolCall: (call, options) => voiceToolRouter.execute(call, options),
  getContext: () => JSON.stringify(voiceConversationFacts()),
});

function readSession() {
  try {
    const fragmentToken = new URLSearchParams(location.hash.slice(1)).get('session');
    if (fragmentToken && /^[a-f0-9]{64}$/.test(fragmentToken)) {
      state.token = fragmentToken;
      sessionStorage.setItem('agentos.session', fragmentToken);
      history.replaceState(null, '', location.pathname + location.search);
    } else state.token = sessionStorage.getItem('agentos.session') || '';
  } catch { /* A denied storage permission must not prevent local interaction. */ }
}
readSession();

function toast(message) {
  $('#toast').textContent = message;
  $('#toast').hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { $('#toast').hidden = true; }, 4200);
}
function respond(message, aloud = false) {
  $('#response-text').textContent = message;
  if (aloud && state.speech && !state.voice.active && state.voice.phase !== 'connecting' && 'speechSynthesis' in window) {
    speechSynthesis.cancel();
    const speech = new SpeechSynthesisUtterance(message);
    speech.lang = 'zh-CN'; speech.rate = .93; speech.volume = .8;
    speechSynthesis.speak(speech);
  }
}
function persistFocus() { storage.set('focus', state.focus); }
function saveNotes() { if (!storage.set('notes', state.notes)) toast('浏览器存储不可用；这次笔记只保留到页面关闭。'); }
function formatTime(date = new Date(), seconds = false) {
  return date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', ...(seconds ? { second: '2-digit' } : {}), hour12: false });
}
function dateLabel(date = new Date()) { return date.toLocaleDateString('zh-CN', { month: 'long', day: 'numeric', weekday: 'long' }); }
function taskText(task) { return task?.intent?.text || task?.text || '未命名意图'; }
function taskId(task) { return typeof task?.id === 'number' ? `task-${task.id}` : String(task?.id || ''); }
function currentNotes() { return state.connected ? state.remoteNotes : state.notes.map(n => n.text); }
function currentTasks() { return state.connected ? state.tasks : state.localLog; }
function modeTag() { return `<span class="scene-tag${state.connected ? '' : ' demo'}">${state.connected ? '已连接本机 AgentOS' : '浏览器本地交互'}</span>`; }
function addLocalActivity(text, message) {
  const task = { id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`, text, state: 'succeeded', message, created_at_ms: Date.now(), steps: [], local: true };
  state.localLog.unshift(task); state.localLog = state.localLog.slice(0, 40);
  storage.set('activity', state.localLog); return task;
}
async function api(path, body) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 14000);
  const mutation = body !== undefined;
  try {
    const result = await fetch(`/api${path}`, {
      method: mutation ? 'POST' : 'GET', cache: 'no-store', signal: controller.signal,
      headers: { 'X-AgentOS-Session': state.token, ...(mutation ? { 'Content-Type': 'application/json' } : {}) },
      ...(mutation ? { body: JSON.stringify(body) } : {}),
    });
    const data = await result.json();
    if (!result.ok || data.error) {
      const error = new Error(data.error?.message || '后台暂时无法回应');
      error.code = data.error?.code; error.unknown = data.error?.outcome_unknown === true;
      throw error;
    }
    return data;
  } catch (error) {
    if (mutation && !error.code) error.unknown = true;
    throw error;
  } finally { clearTimeout(timeout); }
}
function handleError(error) {
  if (error.unknown) {
    state.approval = null;
    respond('连接中断，操作可能已经生效。请先查看任务轨迹，不要直接重发。');
    toast('结果尚不确定：请查询任务或实际笔记，再决定下一步。');
    refreshConnection();
  } else {
    respond(error.message || '暂时没有完成，请稍后再试。');
    toast(error.message || '暂时没有完成');
  }
}

function renderOverview() {
  const now = new Date();
  const hour = now.getHours();
  const greeting = hour < 6 ? 'A MOMENT OF QUIET' : hour < 12 ? 'A NEW DAY, A NEW POSSIBILITY' : hour < 18 ? 'ROOM FOR SOMETHING GOOD' : 'LET THE DAY SETTLE';
  const notes = currentNotes();
  const tasks = currentTasks();
  const active = tasks.filter(t => !terminalStates.has(t.state)).length;
  const note = notes.at(-1);
  return `<section class="ambient-scene" aria-labelledby="welcome-title">
    <div class="ambient-hero"><div class="hero-copy">
      <div class="greeting-date"><span class="eyebrow">${greeting}</span><span>${e(now.toLocaleDateString('zh-CN', { month: '2-digit', day: '2-digit' }).replace('/', '.'))}</span></div>
      <h1 id="welcome-title">留一点空间，<br><span>让想法发生。</span></h1>
      <p>不必寻找应用，也不必安排窗口。<br>只要告诉我，<em>此刻你想做什么。</em></p>
      <div class="hero-actions"><button class="suggestion-chip" data-intent="规划今天">${icon('spark')}看看此刻</button><button class="suggestion-chip" data-intent="开始专注">${icon('leaf')}进入专注</button><button class="suggestion-chip" data-intent="整理灵感">${icon('memory')}整理灵感</button></div>
    </div><div class="orb-space" aria-hidden="true"><canvas class="orb-canvas" id="orb-canvas"></canvas><span class="orb-center-label">A U R A</span><span class="orb-coordinate left">INTENT<br>↓</span><span class="orb-coordinate right">CONTEXT<br>↑</span><div class="orb-description"><span><i></i>在这里，随时倾听</span><span>INTELLIGENCE, AT YOUR PACE</span></div></div></div>
    <div class="section-label"><span>${icon('spark')}为此刻展开</span><small>${state.connected ? '来自本机任务与笔记' : '本地交互 · 接入后台后显示真实任务'}</small></div>
    <div class="ambient-cards">
      <button class="context-card accent" data-scene="focus"><div class="card-top"><span class="card-kicker">${icon('leaf')}一段不被打扰的时间</span><span class="card-micro">FOCUS</span></div><h3>${state.focus.endAt ? '专注，正在发生。' : '把世界，调成安静。'}</h3><p>${state.focus.endAt ? '计时仍在继续，回到你的专注空间。' : '25 分钟，只留给眼前这一件事。'}</p><span class="mini-visual" aria-hidden="true"><i style="--h:7px"></i><i style="--h:13px"></i><i style="--h:22px"></i><i style="--h:16px"></i><i style="--h:27px"></i><i style="--h:18px"></i><i style="--h:12px"></i><i style="--h:6px"></i></span></button>
      <button class="context-card" data-scene="memory"><div class="card-top"><span class="card-kicker">${icon('memory')}思绪的回声</span><span class="card-micro">${notes.length ? `${String(notes.length).padStart(2, '0')} NOTES` : 'MEMORY'}</span></div><h3>${note ? '接住了，这个念头。' : '每个念头，都有回音。'}</h3><p>${e(note ? note.slice(0, 42) + (note.length > 42 ? '…' : '') : '说一句“记一下”，让灵感留在这里。')}</p>${icon('diagonal', 'card-arrow')}</button>
      <button class="context-card" data-scene="activity"><div class="card-top"><span class="card-kicker">${icon('activity')}意图的轨迹</span><span class="card-micro">${String(tasks.length).padStart(2, '0')} INTENTS</span></div><h3>${active ? `${active} 个意图，正在向前。` : tasks.length ? '每一步，都留下痕迹。' : '准备好，与你一起。'}</h3><p>${active ? '查看正在进行的步骤与执行状态。' : tasks.length ? '回顾你的意图、行动与完成的结果。' : '你的意图会在这里，慢慢成为现实。'}</p>${icon('diagonal', 'card-arrow')}</button>
    </div><div class="ambient-caption">没有固定的桌面，只有属于此刻的空间</div>
  </section>`;
}
function sceneHeader(scene) {
  return `<header class="scene-header"><div><span class="eyebrow">${e(scene.eyebrow)}</span><h1>${e(scene.title)}</h1><p>${e(scene.description)}</p></div>${modeTag()}</header>`;
}
function focusRemaining() { return state.focus.endAt ? Math.max(0, Math.ceil((state.focus.endAt - Date.now()) / 1000)) : state.focus.remaining; }
function formatRemaining() { const seconds = focusRemaining(); return `${String(Math.floor(seconds / 60)).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`; }
function renderFocus(scene, withHeader = true) {
  return `${withHeader ? sceneHeader(scene) : ''}<div class="focus-layout"><section class="surface focus-primary" aria-label="专注计时"><div class="focus-ring"><div class="timer" id="focus-timer" role="timer">${formatRemaining()}</div><span class="timer-label" id="focus-status">${state.focus.endAt ? '此刻，只做一件事' : '世界可以等一等'}</span></div><div class="focus-controls"><button class="primary-button" id="focus-toggle">${icon(state.focus.endAt ? 'pause' : 'play')}<span>${state.focus.endAt ? '暂停片刻' : '开始专注'}</span></button><button class="secondary-button" id="focus-reset" aria-label="重置计时">${icon('refresh')}</button></div></section><div class="focus-aside"><section class="surface"><h3>留给自己，一点时间</h3><p>关掉多余的思绪，让注意力落在当下。<br>这个计时器会陪着你，不打扰你。</p><div class="duration-options" aria-label="专注时长">${[25, 45, 60].map(n => `<button data-duration="${n}" class="${state.focus.duration === n ? 'selected' : ''}" aria-pressed="${state.focus.duration === n}">${n} 分钟</button>`).join('')}</div><div class="inline-note">本地计时 · 切换空间后继续，关闭页面后不通知</div></section><section class="surface"><h3>这段时间，想完成什么？</h3><label class="sr-only" for="focus-goal">专注目标</label><textarea id="focus-goal" class="focus-note" maxlength="2000" placeholder="一个小目标，就很好。">${e(state.focus.goal)}</textarea><div class="inline-note">保存在浏览器；开启 AI 布局时作为情景上下文</div></section></div></div>`;
}
function renderMemory(scene, withHeader = true) {
  const notes = currentNotes();
  const panel = scene.panels.find(p => p.type === 'notes');
  const items = panel?.items || notes;
  return `${withHeader ? sceneHeader(scene) : ''}<div class="memory-toolbar"><span class="count-label">${String(items.length).padStart(2, '0')} 个念头 · ${state.connected ? '后台持久笔记' : '保存在这个浏览器'}</span><div><button class="text-button" data-intent="恢复笔记">恢复上次清空</button><button class="primary-button" id="new-note">${icon('spark')}记下灵感</button></div></div>${state.connected && !state.remoteNotesLoaded ? '<p class="system-mode-note">正在从后台读取笔记…</p>' : ''}<div class="notes-grid">${items.length ? items.map((text, i) => `<article class="note-card"><span class="note-index">THOUGHT ${String(i + 1).padStart(2, '0')}</span><p>${e(text)}</p><footer><span>${state.connected ? 'AgentOS · 持久记忆' : '本地 · 灵感'}</span>${state.connected ? icon('memory') : `<button data-delete-note="${e(state.notes[i]?.id)}" aria-label="删除第 ${i + 1} 条笔记">移除</button>`}</footer></article>`).join('') : `<div class="empty-state">${icon('memory')}<h3>给一个念头，留个位置。</h3><p>点击“记下灵感”，或者说“记一下 明天去散步”。<br>不需要文件夹，也不用给它取名字。</p></div>`}</div>${items.length ? '<div class="inline-note"><button class="text-button" data-intent="清空笔记">清空全部笔记（可恢复一次）</button></div>' : ''}`;
}
function statusIcon(task) { return icon(task.state === 'succeeded' ? 'check' : task.state === 'awaiting_confirmation' ? 'lock' : ['failed', 'cancelled', 'interrupted'].includes(task.state) ? 'close' : 'activity'); }
function renderTaskDetail(task) {
  if (!task) return `<div class="empty-state">${icon('orbit')}<h3>每个意图，都有来处。</h3><p>选择一条轨迹，查看它的执行过程。</p></div>`;
  const id = taskId(task);
  const stepRows = (task.steps || []).map((step, index) => `<div class="step"><strong>${String(index + 1).padStart(2, '0')} · ${e(step.capability)}</strong><p>${e(stateLabels[step.state] || step.state)}</p><details><summary class="count-label">参数与结果</summary><pre class="json-output">${e(JSON.stringify({ args: step.args, result: step.result }, null, 2))}</pre></details></div>`).join('');
  const noteResult = (task.steps || []).findLast(s => s.capability === 'notes.list' && s.state === 'succeeded' && Array.isArray(s.result?.notes));
  const confirmation = state.approval?.id === task.id && state.approval?.token === task.confirmation?.token && state.approval.expires_at_ms > Date.now() ? state.approval : null;
  const approval = task.state === 'awaiting_confirmation' ? `<section class="approval-panel"><h3>这一步，需要你的明确批准。</h3><p>请查看具体能力、参数和原因。批准仅针对这一任务的这一步。</p>${confirmation ? `<pre class="json-output">${e(JSON.stringify({ capability: task.steps?.[confirmation.step]?.capability, args: task.steps?.[confirmation.step]?.args, reason: task.message }, null, 2))}</pre><button class="primary-button" id="approve-task" data-task-id="${e(id)}">批准第 ${confirmation.step + 1} 步</button><p>批准请求将在 ${e(formatTime(new Date(confirmation.expires_at_ms)))} 过期。</p>` : '<button class="secondary-button" id="refresh-approval">读取并查看当前批准请求</button>'}</section>` : '';
  return `<div class="detail-kicker">${e(id.toUpperCase())} <span> / ${e(stateLabels[task.state] || task.state)}</span></div><h2>${e(taskText(task))}</h2>${task.message ? `<p class="detail-message">${e(task.message)}</p>` : ''}${stepRows ? `<div class="steps">${stepRows}</div>` : ''}${noteResult ? `<div class="result-notes"><span class="eyebrow">返回的笔记</span>${noteResult.result.notes.length ? noteResult.result.notes.map(n => `<p>${e(n)}</p>`).join('') : '<p>笔记本是空的。</p>'}</div>` : ''}${approval}${state.connected && !terminalStates.has(task.state) ? `<div style="margin-top:20px"><button class="secondary-button" id="cancel-task" data-task-id="${e(id)}">停止后续步骤</button></div>` : ''}${task.local ? '<p class="inline-note">此操作在浏览器内完成，不属于后台任务。</p>' : ''}`;
}
function renderActivity(scene) {
  const tasks = currentTasks();
  return `${sceneHeader(scene)}<div class="activity-toolbar"><span class="count-label">${state.connected ? '后台服务 · 独立于界面持续运行' : '浏览器本地操作记录'}</span><div>${state.connected && state.taskHistory.length ? '<button class="text-button" id="tasks-previous">上一页</button>' : ''}${state.connected && state.nextBefore !== null ? '<button class="text-button" id="tasks-next">更早轨迹</button>' : ''}<button class="secondary-button" id="refresh-tasks">${icon('refresh')}刷新</button></div></div>${tasks.length ? `<div class="task-layout"><div class="task-list" aria-label="任务列表">${tasks.map(task => `<button class="task-item ${taskId(task) === taskId(state.selected) ? 'selected' : ''}" data-select-task="${e(taskId(task))}" aria-pressed="${taskId(task) === taskId(state.selected)}"><span class="task-state-icon ${e(task.state)}">${statusIcon(task)}</span><span class="task-summary"><strong>${e(taskText(task))}</strong><small>${e(stateLabels[task.state] || task.state)} · ${e(formatTime(new Date(task.created_at_ms || Date.now())))}</small></span></button>`).join('')}</div><section class="surface task-detail" aria-label="任务详情">${renderTaskDetail(state.selected)}</section></div>` : `<div class="empty-state">${icon('activity')}<h3>故事，从你的第一个意图开始。</h3><p>${state.connected ? '试着说：“记一下 明天去散步”。' : '试着记下一条笔记，或者开启一段专注。'}<br>每一步完成后，都会在这里留下痕迹。</p></div>`}`;
}
function renderClock(scene, withHeader = true) { return `${withHeader ? sceneHeader(scene) : ''}<section class="clock-scene surface"><span class="eyebrow">THE ONLY MOMENT IS NOW</span><div class="big-clock" id="big-clock">${formatTime(new Date(), true)}</div><div class="clock-date">${e(dateLabel())}</div><p class="inline-note">设备本地时间 · ${e(Intl.DateTimeFormat().resolvedOptions().timeZone)}</p></section>`; }
function renderHelp(scene) {
  const examples = [ ['记一下 明天去散步', '记录一个念头，不必打开应用。'], ['看看笔记', '让记忆成为眼前的工作空间。'], ['开始专注', '用一段安静的时间，完成一件小事。'], ['任务进展', '查看真正执行了什么，结果如何。'], ['现在几点', '让时间，成为整个画面。'], ['规划今天', '回到此刻的笔记、任务和专注。'] ];
  return `${sceneHeader(scene)}<div class="help-grid">${examples.map(([text, description]) => `<button class="help-example" data-intent="${e(text)}"><strong>“${e(text)}”</strong><p>${e(description)}</p></button>`).join('')}</div><p class="system-mode-note">${state.connected ? `当前后台授权 ${state.capabilities.length} 个能力：${state.capabilities.map(c => e(c.name)).join('、') || '正在读取'}。专注计时和界面导航在浏览器内完成。` : '当前是浏览器本地交互模式：支持笔记、专注、时间和本地轨迹。连接后台后可提交真实任务。'}</p>`;
}

function sceneFacts() {
  return { kind: state.scene === 'note' ? 'memory' : state.scene, task: state.selected,
    notes: currentNotes(), focus: { active: !!state.focus.endAt, duration: state.focus.duration * 60 },
    capabilities: state.capabilities };
}
function layoutContext() {
  const kind = state.scene === 'note' ? 'memory' : state.scene;
  const focus = { active: !!state.focus.endAt, duration: state.focus.duration * 60, goal: state.focus.goal.slice(0, 1000) };
  const notes = currentNotes().slice(-12).map(note => note.slice(0, 500));
  const viewport = { width: Math.max(240, Math.min(10000, innerWidth)), height: Math.max(240, Math.min(10000, innerHeight)) };
  const tasks = currentTasks().slice(0, 8);
  const key = JSON.stringify({
    kind, intent: state.lastIntent, connected: state.connected, revision: state.revision,
    selected: state.selected ? [taskId(state.selected), state.selected.state, state.selected.updated_at_ms] : null,
    tasks: tasks.map(task => [taskId(task), task.state, task.updated_at_ms]), notes,
    notesKnown: state.connected ? state.remoteNotesLoaded : true, focus,
    capabilities: state.capabilities.map(capability => capability.name),
    viewport: [Math.round(viewport.width / 80), Math.round(viewport.height / 80)],
    hour: Math.floor(Date.now() / 3600000),
  });
  // Drafts, microphone audio, confirmation payloads and credentials are never
  // layout context. Canonical daemon facts are fetched again by the gateway.
  return { key, context: { kind, intent: state.lastIntent.slice(0, 2000),
    selected_task_id: state.connected && Number.isSafeInteger(state.selected?.id) ? state.selected.id : null,
    local: { notes, focus, viewport,
      time: { iso: new Date().toISOString(), timezone: Intl.DateTimeFormat().resolvedOptions().timeZone },
      recent_intents: [...state.voiceCaptions.filter(item => item.role === 'user' && item.final).slice(-3).map(item => item.text.slice(0, 240)),
        ...tasks.map(task => taskText(task).slice(0, 240))].slice(0, 8) } } };
}
async function requestModelLayout(context, { signal }) {
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (signal.aborted) controller.abort(); else signal.addEventListener('abort', abort, { once: true });
  const timer = setTimeout(abort, 30000);
  try {
    const result = await fetch('/api/layout', { method: 'POST', cache: 'no-store', signal: controller.signal,
      headers: { 'Content-Type': 'application/json', 'X-AgentOS-Session': state.token }, body: JSON.stringify(context) });
    const response = await result.json();
    if (!result.ok || response.error) {
      const messages = {
        layout_unavailable: '模型未配置，使用基础布局', layout_busy: '模型正在处理其他情景，使用基础布局',
        layout_timeout: '模型响应超时，使用基础布局', layout_provider_error: '模型服务暂不可用，使用基础布局',
        layout_refused: '模型未生成此情景，使用基础布局', session_required: '请用新的启动链接连接模型',
      };
      throw new Error(messages[response.error?.code] || '模型布局不可用，使用基础布局');
    }
    const scene = validateModelScene(response.scene, context.kind);
    if (response.source !== 'model' || !scene) throw new Error('布局校验未通过，使用基础布局');
    return { ...response, scene };
  } catch (error) {
    if (controller.signal.aborted && !signal.aborted) throw new Error('模型响应超时，使用基础布局');
    throw error;
  } finally { clearTimeout(timer); signal.removeEventListener('abort', abort); }
}
function queueLayout() {
  if (!state.layoutEnabled || !state.layoutAvailable || !state.token || state.busy || refreshInFlight || state.connection === 'starting' || document.hidden) return;
  if (state.connected && !state.remoteNotesLoaded && state.notebookTasks.size) return;
  const { key, context } = layoutContext();
  layoutCoordinator.update(key, context);
}
function updateLayoutIndicator() {
  const ready = state.layoutEnabled && state.generatedScene && state.generatedKey === layoutContext().key;
  const label = !state.layoutEnabled ? '基础情景画布'
    : !state.layoutAvailable ? '基础情景画布'
    : state.layoutStatus === 'generating' ? 'AI 正在组织空间'
    : ready ? 'AI 情景画布' : state.layoutStatus === 'fallback' ? '基础布局 · 可重试' : 'AI 情景就绪';
  $('#workspace-mode').textContent = label;
  $('#workspace-mode').title = state.layoutError || (ready ? '内容、组件顺序和布局由模型选择，任务数据来自后台' : label);
  const retry = $('#regenerate-layout');
  if (retry) retry.disabled = !state.layoutEnabled || !state.layoutAvailable || state.layoutStatus === 'generating';
  const details = $('#layout-details');
  if (details) details.textContent = !state.layoutEnabled ? 'AI 布局已关闭，使用本地情景规则。'
    : state.layoutAvailable ? `${state.layoutModel} · ${state.layoutError || '根据任务、笔记摘要与当前空间生成布局；失败时保留基础界面。'}`
    : '模型尚未连接；启动 Shell 时需要可用的模型配置和本次访问链接。';
}
function renderWhenUnoccupied() {
  if (canvasDirty && !$('#scene-content').contains(document.activeElement) && !state.voice.userSpeaking) render();
}
async function refreshLayoutAvailability() {
  if (layoutStatusInFlight || !state.token) { updateLayoutIndicator(); return; }
  layoutStatusInFlight = true;
  try {
    const status = await api('/layout/status');
    state.layoutAvailable = status.enabled === true && status.protocol_version === 1;
    state.layoutModel = typeof status.model === 'string' ? status.model : '';
    if (state.layoutAvailable) queueLayout();
  } catch { state.layoutAvailable = false; }
  finally { layoutStatusInFlight = false; updateLayoutIndicator(); }
}

function renderGeneratedActivity() {
  const tasks = currentTasks();
  return `<div class="activity-toolbar"><span class="count-label">${state.connected ? '后台真实任务' : '浏览器本地操作'}</span><div>${state.connected && state.taskHistory.length ? '<button class="text-button" id="tasks-previous">上一页</button>' : ''}${state.connected && state.nextBefore !== null ? '<button class="text-button" id="tasks-next">更早轨迹</button>' : ''}<button class="secondary-button" id="refresh-tasks">${icon('refresh')}刷新</button></div></div><div class="task-list">${tasks.length ? tasks.map(task => `<button class="task-item ${taskId(task) === taskId(state.selected) ? 'selected' : ''}" data-select-task="${e(taskId(task))}" aria-pressed="${taskId(task) === taskId(state.selected)}"><span class="task-state-icon ${e(task.state)}">${statusIcon(task)}</span><span class="task-summary"><strong>${e(taskText(task))}</strong><small>${e(stateLabels[task.state] || task.state)} · ${e(formatTime(new Date(task.created_at_ms || Date.now())))}</small></span></button>`).join('') : '<p class="inline-note">还没有执行记录。从一句话开始。</p>'}</div>`;
}
function renderGeneratedPanel(panel, scene) {
  const typeLabels = { welcome: '此刻', insight: 'AI 情景理解', suggestions: '接下来', notes: '真实笔记', task: '真实执行状态', activity: '意图轨迹', clock: '设备本地时间', focus: '专注空间', capabilities: '可用能力' };
  const header = `<div class="generated-panel-heading"><span>${e(typeLabels[panel.type])}</span><span>${String(panel.priority).padStart(2, '0')}</span></div>`;
  const caption = `<h2 class="generated-panel-title">${e(panel.title)}</h2>${panel.description ? `<p class="generated-panel-description">${e(panel.description)}</p>` : ''}`;
  let body = '';
  switch (panel.type) {
    case 'welcome':
      body = `<div class="generated-welcome"><div class="generated-welcome-copy"><span class="eyebrow">${e(dateLabel())}</span>${caption}</div><div class="orb-space" aria-hidden="true"><canvas id="orb-canvas" class="orb-canvas"></canvas><span class="orb-center-label">A U R A</span></div></div>`;
      break;
    case 'insight': body = `${caption}<div class="generated-source">模型建议 · 结合当前上下文</div>`; break;
    case 'suggestions': body = `${caption}<div class="generated-suggestions">${(panel.items || []).map(text => `<button class="suggestion-chip" data-intent="${e(text)}">${icon('spark')}${e(text)}${icon('arrow')}</button>`).join('')}</div>`; break;
    case 'notes': body = `${caption}${renderMemory(scene, false)}`; break;
    case 'task': body = `${caption}<div class="task-detail">${renderTaskDetail(state.selected)}</div>`; break;
    case 'activity': body = `${caption}${renderGeneratedActivity()}`; break;
    case 'clock': body = `${caption}${renderClock(scene, false)}`; break;
    case 'focus': body = `${caption}${renderFocus(scene, false)}`; break;
    case 'capabilities':
      body = `${caption}<div class="generated-capabilities">${state.capabilities.length ? state.capabilities.map(capability => `<article><strong>${e(capability.name)}</strong><p>${e(capability.description)}</p></article>`).join('') : '<p class="inline-note">连接后台后，将显示实际授权的能力。当前可使用浏览器笔记与专注计时。</p>'}</div>`;
      break;
  }
  return `<section class="generated-panel panel-${e(panel.type)}" data-span="${panel.span || 1}" data-panel-type="${e(panel.type)}" aria-label="${e(panel.title || typeLabels[panel.type])}">${header}${body}</section>`;
}
function renderGeneratedScene(scene) {
  return `${sceneHeader(scene)}<div class="generated-grid" data-layout="${e(scene.layout)}" data-source="model">${scene.panels.map(panel => renderGeneratedPanel(panel, scene)).join('')}</div><div class="generated-footnote">${icon('spark')}空间由 AI 组织 · 任务、笔记与批准来自真实状态<button id="regenerate-scene" class="text-button">重新组织</button></div>`;
}
function stopOrb() { cancelAnimationFrame(orbFrame); orbResizeObserver?.disconnect(); orbResizeObserver = null; orbRedraw = null; }
function render() {
  stopOrb();
  canvasDirty = false;
  const kind = state.scene === 'note' ? 'memory' : state.scene;
  const facts = sceneFacts();
  const modelScene = state.layoutEnabled && state.generatedKey === layoutContext().key
    ? bindModelScene(state.generatedScene, facts) : null;
  const scene = modelScene || validateScene(composeScene(facts));
  const labels = sceneLabels[kind] || sceneLabels.overview;
  $('#scene-content').dataset.layout = scene.layout;
  $('#scene-number').textContent = labels[0]; $('#scene-label').textContent = labels[1];
  for (const button of document.querySelectorAll('.rail-navigation .rail-button')) {
    const selected = button.dataset.scene === kind || (kind === 'task' && button.dataset.scene === 'activity');
    button.classList.toggle('active', selected);
    if (selected) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
  }
  $('#scene-content').innerHTML = modelScene ? renderGeneratedScene(scene) : kind === 'overview' ? renderOverview() : kind === 'focus' ? renderFocus(scene) : kind === 'memory' ? renderMemory(scene) : ['activity', 'task'].includes(kind) ? renderActivity(scene) : kind === 'clock' ? renderClock(scene) : renderHelp(scene);
  $('#activity-dot').hidden = !state.tasks.some(t => !terminalStates.has(t.state));
  if ($('#orb-canvas')) startOrb();
  updateLayoutIndicator();
  queueLayout();
  syncVoiceContext();
}
async function navigate(kind, { load = true } = {}) {
  const requestVersion = ++sceneRequestVersion;
  state.scene = kind; state.approval = null; render();
  $('#main').scrollTop = 0;
  if (kind === 'memory' && load && state.connected) {
    try { await submitRemote('看看笔记', 'memory'); } catch (error) { handleError(error); }
  } else if ((kind === 'activity' || kind === 'help') && state.connected) {
    try {
      if (kind === 'help') state.capabilities = (await api('/capabilities')).capabilities || [];
      else await refreshTasks();
      if (requestVersion === sceneRequestVersion) render();
    } catch (error) { handleError(error); }
  }
}

function startOrb() {
  const canvas = $('#orb-canvas'); if (!canvas) return;
  const ctx = canvas.getContext('2d'); if (!ctx) return;
  let width = 0, height = 0;
  const particles = [];
  for (let i = 0; i < 1350; i++) {
    const y = 1 - (i / 1349) * 2;
    const radius = Math.sqrt(1 - y * y);
    const theta = i * Math.PI * (3 - Math.sqrt(5));
    particles.push([Math.cos(theta) * radius, y, Math.sin(theta) * radius]);
  }
  let lastDraw = 0;
  function draw(time = 0) {
    if (state.motion && time - lastDraw < 32) { orbFrame = requestAnimationFrame(draw); return; }
    lastDraw = time;
    ctx.clearRect(0, 0, width, height);
    const cx = width * .50, cy = height * .46;
    const radius = Math.min(width * .31, height * .35);
    const glow = ctx.createRadialGradient(cx, cy, radius * .1, cx, cy, radius * 1.6);
    glow.addColorStop(0, '#b9d4950e'); glow.addColorStop(.55, '#94b97408'); glow.addColorStop(1, '#819c6700');
    ctx.fillStyle = glow; ctx.fillRect(0, 0, width, height);
    const angle = state.motion ? time * .000045 : .7;
    const projected = particles.map(([x, y, z], i) => {
      const rx = x * Math.cos(angle) + z * Math.sin(angle), rz = z * Math.cos(angle) - x * Math.sin(angle);
      const ry = y * Math.cos(.28) - rz * Math.sin(.28), zz = y * Math.sin(.28) + rz * Math.cos(.28);
      const ripple = 1 + .025 * Math.sin(i * .11 + (state.motion ? time * .0005 : 0));
      return [cx + rx * radius * ripple, cy + ry * radius * ripple, zz];
    }).sort((a, b) => a[2] - b[2]);
    for (const [x, y, z] of projected) {
      ctx.beginPath(); ctx.arc(x, y, z > .6 ? .9 : .6, 0, Math.PI * 2);
      ctx.fillStyle = `rgba(186,207,151,${.08 + (z + 1) * .22})`; ctx.fill();
    }
    for (let i = 0; i < 4; i++) {
      ctx.save(); ctx.translate(cx, cy); ctx.rotate(-.48 + i * .33);
      ctx.beginPath(); ctx.ellipse(0, 0, radius * (1.27 + i * .1), radius * (.44 + i * .13), 0, 0, Math.PI * 2);
      ctx.strokeStyle = i === 1 ? '#b6cb8d2f' : '#a7bb8615'; ctx.lineWidth = .6; ctx.stroke();
      if (i === 1) {
        const t = -angle * 1.5 + .4;
        ctx.beginPath(); ctx.arc(Math.cos(t) * radius * 1.37, Math.sin(t) * radius * .57, 2, 0, Math.PI * 2);
        ctx.fillStyle = '#d9ddad'; ctx.shadowBlur = 12; ctx.shadowColor = '#c7da9d'; ctx.fill();
      }
      ctx.restore();
    }
    ctx.strokeStyle = '#a1b68710'; ctx.lineWidth = .5;
    ctx.beginPath(); ctx.moveTo(cx - radius * 1.8, cy); ctx.lineTo(cx + radius * 1.8, cy); ctx.stroke();
    ctx.beginPath(); ctx.moveTo(cx, cy - radius * 1.4); ctx.lineTo(cx, cy + radius * 1.4); ctx.stroke();
    for (const [dx, dy] of [[-1.45, -.9], [1.35, .73], [-1.1, 1.05], [1.08, -1.05]]) {
      ctx.strokeStyle = '#b5c99460'; ctx.beginPath(); ctx.moveTo(cx + radius * dx - 3, cy + radius * dy); ctx.lineTo(cx + radius * dx + 3, cy + radius * dy); ctx.moveTo(cx + radius * dx, cy + radius * dy - 3); ctx.lineTo(cx + radius * dx, cy + radius * dy + 3); ctx.stroke();
    }
    if (state.motion && !document.hidden) orbFrame = requestAnimationFrame(draw);
  }
  function resize() {
    const rect = canvas.getBoundingClientRect(); width = rect.width; height = rect.height;
    const dpr = Math.min(devicePixelRatio || 1, 2); canvas.width = width * dpr; canvas.height = height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0); cancelAnimationFrame(orbFrame); draw(performance.now());
  }
  orbRedraw = resize; orbResizeObserver = new ResizeObserver(resize); orbResizeObserver.observe(canvas); resize();
}

async function refreshTasks({ before = state.currentBefore } = {}) {
  const page = await api(`/tasks?limit=20${before === null ? '' : `&before=${encodeURIComponent(before)}`}`);
  state.tasks = page.tasks || []; state.nextBefore = page.next_before ?? null;
  if (Number.isSafeInteger(page.revision)) state.revision = page.revision;
  if (state.selected && !state.selected.local) {
    const data = await api(`/tasks/${encodeURIComponent(taskId(state.selected))}`);
    if (data.task?.id === state.selected?.id) { state.selected = data.task; processTask(data.task); }
  }
}
function processTask(task) {
  observeNotebookTask(task);
  if (task.state === 'awaiting_confirmation' && `${task.id}:${task.confirmation?.token}` !== lastAnnouncedConfirmation) {
    lastAnnouncedConfirmation = `${task.id}:${task.confirmation?.token}`;
    respond('计划已暂停。请在任务详情里查看并批准这一具体步骤。', true);
  }
  else if (terminalStates.has(task.state) && `${task.id}:${task.state}` !== lastAnnouncedTask) {
    lastAnnouncedTask = `${task.id}:${task.state}`;
    respond(task.state === 'succeeded' ? task.message || '已经完成。结果留在你的工作空间。' : task.message || stateLabels[task.state], true);
  }
  if (!task.confirmation || state.approval?.token !== task.confirmation.token) state.approval = null;
}
async function selectTask(id, { readApproval = false } = {}) {
  const version = ++taskSelectionVersion; state.approval = null;
  if (!state.connected) { state.selected = state.localLog.find(t => t.id === id) || null; render(); return; }
  try {
    const data = await api(`/tasks/${encodeURIComponent(id)}`);
    if (version !== taskSelectionVersion) return;
    state.selected = data.task; processTask(data.task);
    if (readApproval && data.task.state === 'awaiting_confirmation' && data.task.confirmation?.expires_at_ms > Date.now()) state.approval = { id: data.task.id, ...data.task.confirmation };
    render();
  } catch (error) { handleError(error); }
}
async function submitRemote(text, preferredScene = 'task') {
  const version = sceneRequestVersion;
  const changesScene = state.scene !== preferredScene;
  const readVersion = text === '看看笔记' ? ++state.notebookReadVersion : null;
  if (readVersion !== null) state.remoteNotesLoaded = false;
  const data = await api('/intent', { text });
  trackNotebookTask(data.task, readVersion);
  state.selected = data.task; state.approval = null; state.currentBefore = null; state.taskHistory = [];
  if (version === sceneRequestVersion) state.scene = preferredScene;
  respond('意图已收到。后台正在规划，进展会在这里展开。');
  try { await refreshTasks({ before: null }); }
  catch { respond('任务已被后台接收，当前无法刷新结果。请查看任务轨迹，不要重复提交。'); }
  render();
  if (changesScene && version === sceneRequestVersion) $('#main').scrollTop = 0;
  return state.selected?.id === data.task.id ? state.selected : data.task;
}

// Only tasks accepted from this browser enter this tracker. Viewing an old
// notes.list/clear task must never replace the current notebook snapshot.
function trackNotebookTask(task, readVersion = null) {
  if (!task || !Number.isSafeInteger(task.id)) return;
  state.notebookTasks.set(task.id, { readVersion });
  observeNotebookTask(task);
}
function observeNotebookTask(task) {
  const tracked = state.notebookTasks.get(task?.id);
  if (!tracked || !terminalStates.has(task.state)) return false;
  state.notebookTasks.delete(task.id);
  if (tracked.readVersion !== null) {
    const result = (task.steps || []).findLast(step => step.capability === 'notes.list'
      && step.state === 'succeeded' && Array.isArray(step.result?.notes));
    if (result && tracked.readVersion === state.notebookReadVersion) {
      state.remoteNotes = result.result.notes.filter(note => typeof note === 'string');
      state.remoteNotesLoaded = true;
      return true;
    }
    return false;
  }
  if ((task.steps || []).some(step => step.state === 'succeeded'
    && ['notes.append', 'notes.clear', 'notes.restore'].includes(step.capability))) {
    state.remoteNotesLoaded = false;
    state.notebookRefreshNeeded = true;
  }
  return false;
}
async function refreshNotebookTasks() {
  let changed = false;
  // Independent reads follow live writes even when the user selects another
  // task, changes space, or pages away from the task in the activity list.
  for (const id of [...state.notebookTasks.keys()]) {
    const data = await api(`/tasks/task-${id}`);
    changed = observeNotebookTask(data.task) || changed;
  }
  if (state.notebookRefreshNeeded) {
    // Clear before sending. A lost response may already have created a read
    // task; never replay it after reconnecting. A user can explicitly refresh.
    state.notebookRefreshNeeded = false;
    const readVersion = ++state.notebookReadVersion;
    const data = await api('/intent', { text: '看看笔记' });
    trackNotebookTask(data.task, readVersion);
  }
  return changed;
}
async function submitIntent(rawText, { fromVoice = false } = {}) {
  const text = String(rawText).trim();
  if (!text) return { ok: false, error: 'empty_intent' };
  if (state.busy) return { ok: false, error: 'busy', message: '上一项意图仍在提交，尚未发送这次请求。' };
  const submittedFromInput = !fromVoice && $('#intent-input').value.trim() === text;
  if (new TextEncoder().encode(text).length > 65536) { toast('这个意图太长，请缩短后再提交。'); return { ok: false, error: 'intent_too_long' }; }
  const intent = classifyIntent(text);
  state.lastIntent = text;
  let submittedTask = null;
  let supported = true;
  state.busy = true; $('#intent-form button[type="submit"]').disabled = true;
  try {
    if (['overview', 'focus', 'clock', 'help', 'activity'].includes(intent.kind)) {
      await navigate(intent.kind);
      const messages = { overview: '为你展开此刻的空间。', focus: '把注意力，留给一件事。准备好后，点一下开始专注。', clock: `现在是 ${formatTime()}。`, help: '从一句话开始。这里是我目前能为你做的事。', activity: '这是你的意图轨迹，每一步都可以回看。' };
      respond(messages[intent.kind], true);
    } else if (state.connected) {
      submittedTask = await submitRemote(intent.kind === 'memory' ? '看看笔记' : text, intent.kind === 'memory' ? 'memory' : 'task');
    } else if (intent.kind === 'memory') {
      await navigate('memory', { load: false }); respond(`这里留着你的 ${state.notes.length} 个念头。`, true);
    } else if (intent.kind === 'note') {
      const backup = storage.get('notesBackup', []);
      if (state.notes.length + (Array.isArray(backup) ? backup.length : 0) >= 100) { toast('本地最多保存 100 条笔记（含恢复快照），请先恢复或整理现有笔记。'); return { ok: false, error: 'notebook_full' }; }
      state.notes.push({ id: `${Date.now()}-${Math.random().toString(36).slice(2, 7)}`, text: intent.note, at: Date.now() }); saveNotes();
      addLocalActivity(text, '已保存在当前浏览器的笔记里。'); await navigate('memory', { load: false }); respond('接住了。这个念头已经留在这里。', true);
    } else if (/^(?:请)?清空(?:全部)?笔记[。!！]?$/u.test(text)) {
      const backup = storage.get('notesBackup', null);
      if (backup?.length && state.notes.length) { respond('还有一份尚未恢复的笔记。请先恢复，再清空，以免覆盖它。'); return { ok: false, error: 'restore_before_clear' }; }
      if (state.notes.length) {
        if (!storage.set('notesBackup', state.notes)) { respond('浏览器无法保存恢复快照，笔记仍保留原样。'); return { ok: false, error: 'backup_unavailable' }; }
        state.notes = []; saveNotes(); addLocalActivity(text, '笔记已清空，保留了一次恢复快照。');
      }
      await navigate('memory', { load: false }); respond('空间清出来了。说“恢复笔记”，就能找回。', true);
    } else if (/^(?:请)?恢复笔记[。!！]?$/u.test(text)) {
      const backup = storage.get('notesBackup', []);
      if (Array.isArray(backup) && backup.length) {
        const restored = [...new Map([...backup.filter(n => n && typeof n.text === 'string' && typeof n.id === 'string'), ...state.notes].map(note => [note.id, note])).values()];
        if (!storage.set('notes', restored)) { respond('浏览器暂时无法保存恢复内容。原笔记和恢复快照仍然保留。'); return { ok: false, error: 'storage_unavailable' }; }
        state.notes = restored; storage.set('notesBackup', null); addLocalActivity(text, '已恢复笔记，保留清空后新增的内容。');
        respond('之前的念头，都回来了。', true);
      } else respond('没有等待恢复的笔记。');
      await navigate('memory', { load: false });
    } else {
      supported = false;
      await navigate('help', { load: false });
      respond('本地模式能记笔记、计时和整理空间。连接 AgentOS 后，我可以把更多意图交给后台规划。');
    }
    if (submittedFromInput && $('#intent-input').value.trim() === text) $('#intent-input').value = '';
    return { ok: supported, ...(supported ? {} : { error: 'unsupported_offline' }),
      mode: state.connected ? 'daemon' : 'browser', scene: state.scene,
      message: $('#response-text').textContent,
      ...(submittedTask ? { accepted: true, task: voiceTaskFacts(submittedTask) } : { notes_count: currentNotes().length }) };
  } catch (error) {
    handleError(error);
    return { ok: false, error: error.code || 'connection_error', outcome_unknown: error.unknown === true,
      message: error.unknown ? '操作可能已生效，先查询实际任务，不要重新提交。' : '未收到任务接收确认，请检查连接。' };
  }
  finally { state.busy = false; $('#intent-form button[type="submit"]').disabled = false; queueLayout(); syncVoiceContext(); }
}

function setConnection(connected, code) {
  const changed = state.connected !== connected;
  state.connected = connected; state.connection = code;
  $('#connection-button').dataset.state = connected ? 'online' : 'offline';
  $('#connection-label').textContent = connected ? 'AgentOS 已连接' : '本地交互';
  $('#connection-details').textContent = connected ? '已连接当前用户的 AgentOS 后台。意图、任务和后台笔记来自真实服务。浏览器笔记保持独立，不自动同步。' : code === 'session_required' || !state.token ? '尚未授权后台连接。启动 agentos-shell 后，请打开终端输出的完整访问链接。当前可体验浏览器本地笔记、专注与情景布局。' : '未连接 agentosd。请使用相同 --state-dir 启动后台服务。当前可体验浏览器本地笔记、专注与情景布局。';
  if (!connected) { state.approval = null; state.remoteNotes = []; state.remoteNotesLoaded = false; state.revision = null; if (changed) state.selected = null; }
  if (changed) render();
}
async function refreshConnection() {
  if (refreshInFlight || document.hidden) return;
  if (!state.token) { setConnection(false, 'session_required'); return; }
  refreshInFlight = true;
  try {
    const status = await api('/status');
    if (Number.isSafeInteger(status.revision)) state.revision = status.revision;
    const wasConnected = state.connected; setConnection(true, 'online');
    const before = JSON.stringify({ tasks: state.tasks, selected: state.selected, notes: state.remoteNotes, notesLoaded: state.remoteNotesLoaded });
    await refreshTasks();
    if (!wasConnected) state.notebookRefreshNeeded = true;
    await refreshNotebookTasks();
    if (!wasConnected) {
      state.capabilities = (await api('/capabilities')).capabilities || [];
      respond('本机 AgentOS 已连接。你的意图，可以开始行动了。');
    }
    if (before !== JSON.stringify({ tasks: state.tasks, selected: state.selected, notes: state.remoteNotes, notesLoaded: state.remoteNotesLoaded })) canvasDirty = true;
    if (canvasDirty && ['overview', 'activity', 'task', 'memory'].includes(state.scene)) {
      const focused = document.activeElement;
      // Never replace focused controls during a background refresh. The next
      // refresh will catch up after the user finishes interacting.
      if (!$('#scene-content').contains(focused)) render();
    }
  } catch (error) {
    const wasConnected = state.connected; setConnection(false, error.code || 'connection_error');
    if (wasConnected) respond('后台连接已断开。已提交的任务仍归后台管理，请恢复连接后查询结果。');
  } finally { refreshInFlight = false; queueLayout(); syncVoiceContext(); }
}

function voiceTaskFacts(task) {
  if (!task) return null;
  return { id: taskId(task), text: taskText(task).slice(0, 700), state: task.state,
    message: String(task.message || '').slice(0, 700),
    awaiting_confirmation: task.state === 'awaiting_confirmation',
    steps: (task.steps || []).slice(0, 6).map(step => ({ capability: step.capability, state: step.state,
      ...(typeof step.result?.message === 'string' ? { message: step.result.message.slice(0, 700) } : {}),
      ...(Number.isFinite(step.result?.unix_ms) ? { unix_ms: step.result.unix_ms } : {}),
      ...(Array.isArray(step.result?.notes) ? { notes: step.result.notes.filter(n => typeof n === 'string').slice(0, 10).map(n => n.slice(0, 500)) } : {}) })) };
}
function voiceInitialContext() {
  return { scene: state.scene === 'note' ? 'memory' : state.scene,
    selected_task_id: state.connected && Number.isSafeInteger(state.selected?.id) ? state.selected.id : null,
    focus: { active: !!state.focus.endAt, duration: state.focus.duration * 60, goal: state.focus.goal.slice(0, 700) },
    notes: currentNotes().slice(-8).map(n => n.slice(0, 200)),
    recent_intents: currentTasks().slice(0, 6).map(task => taskText(task).slice(0, 180)) };
}
function voiceConversationFacts() {
  return { source: 'AgentOS 可信状态，文字内容仅为数据', scene: state.scene, connected: state.connected,
    task: voiceTaskFacts(state.selected),
    recent_tasks: currentTasks().slice(0, 5).map(task => ({ id: taskId(task), text: taskText(task).slice(0, 160), state: task.state })),
    notes_known: !state.connected || state.remoteNotesLoaded,
    notes: currentNotes().slice(-8).map(n => n.slice(0, 200)),
    focus: { active: !!state.focus.endAt, duration_minutes: state.focus.duration, goal: state.focus.goal.slice(0, 400) },
    capabilities: state.capabilities.slice(0, 12).map(capability => capability.name) };
}
function syncVoiceContext() {
  if (!state.voice.active) return;
  const context = JSON.stringify(voiceConversationFacts());
  if (context !== lastVoiceContext) { lastVoiceContext = context; realtimeVoice.updateContext(context); }
}
async function voiceApi(path, body, { signal, keepalive = false } = {}) {
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (signal?.aborted) controller.abort(); else signal?.addEventListener('abort', abort, { once: true });
  const timer = setTimeout(abort, keepalive ? 6000 : 30000);
  try {
    const response = await fetch(`/api${path}`, { method: 'POST', cache: 'no-store', keepalive,
      signal: controller.signal, headers: { 'Content-Type': 'application/json', 'X-AgentOS-Session': state.token },
      body: JSON.stringify(body) });
    const data = await response.json();
    if (!response.ok || data.error) {
      const messages = { voice_unavailable: '实时语音尚未配置，可继续使用键盘。',
        voice_busy: '已有语音连接正在建立，请稍后重试。', voice_timeout: '实时语音连接超时，请重新开启。',
        voice_provider_error: '实时语音服务暂不可用，请稍后重试。', session_required: '请打开终端输出的完整链接，再开启实时语音。' };
      const error = new Error(messages[data.error?.code] || '实时语音连接暂不可用，请重新开启。');
      error.voiceMessage = error.message;
      throw error;
    }
    return data;
  } finally { clearTimeout(timer); signal?.removeEventListener('abort', abort); }
}
function createVoiceSession({ sdp, signal }) { return voiceApi('/voice/session', { sdp, context: voiceInitialContext() }, { signal }); }
function supportsRealtimeVoice() { return !!(window.isSecureContext && window.RTCPeerConnection && navigator.mediaDevices?.getUserMedia); }
async function refreshVoiceAvailability() {
  if (voiceStatusInFlight || !state.token) { updateVoiceControls(); return; }
  voiceStatusInFlight = true;
  try {
    const status = await api('/voice/status');
    state.voiceAvailable = status.enabled === true && status.protocol_version === 1;
    state.voiceModel = typeof status.model === 'string' ? status.model : '';
  } catch { state.voiceAvailable = false; }
  finally { voiceStatusInFlight = false; updateVoiceControls(); }
}
function updateVoiceState(snapshot) {
  state.voice = { ...snapshot };
  updateVoiceControls();
  if (!snapshot.userSpeaking) renderWhenUnoccupied();
  if (snapshot.active) syncVoiceContext();
}
function updateVoiceControls() {
  const { phase, active, muted, userSpeaking, assistantSpeaking } = state.voice;
  const engaged = active || phase === 'connecting';
  const labels = { idle: '按下麦克风，开始实时对话', connecting: '正在建立实时语音连接…',
    listening: muted ? '麦克风已静音 · 对话保持连接' : '持续聆听 · 说完即可回应',
    user_speaking: '我在听 · 说完即可回应', thinking: '正在思考 · 可继续说话',
    speaking: 'AI 正在回应 · 开口即可打断', error: '语音已断开 · 可重新开启' };
  const label = labels[phase] || '实时语音';
  document.body.classList.toggle('voice-active', engaged);
  document.body.classList.toggle('is-listening', !!userSpeaking);
  document.body.classList.toggle('voice-speaking', !!assistantSpeaking);
  $('#live-voice-panel').hidden = phase === 'idle';
  $('#voice-session-status').textContent = label;
  $('#voice-mode-label').textContent = engaged ? label : labels.idle;
  $('#mic-button').setAttribute('aria-label', engaged ? '结束实时对话' : '开始实时语音对话');
  $('#mic-button').title = `${engaged ? '结束实时对话' : '开始实时语音对话'} · Alt+V`;
  $('#mic-button use').setAttribute('href', engaged ? '#i-close' : '#i-mic');
  $('#voice-mute').disabled = !active;
  $('#voice-mute').setAttribute('aria-pressed', String(!!muted));
  $('#voice-mute').setAttribute('aria-label', muted ? '取消麦克风静音' : '静音麦克风');
  $('#voice-mute').title = muted ? '取消麦克风静音' : '静音麦克风';
  $('#voice-mute use').setAttribute('href', muted ? '#i-mic-off' : '#i-mic');
  $('#voice-interrupt').disabled = !active || (!assistantSpeaking && phase !== 'thinking');
  $('#intent-input').placeholder = engaged ? '也可以用键盘输入，语音会持续连接…' : '说点什么，或在这里输入…';
  const error = state.voice.error;
  $('#voice-error').textContent = error ? String(error.message || error).slice(0, 240) : '';
  $('#voice-error').hidden = !error;
  $('#voice-settings-status').textContent = !supportsRealtimeVoice() ? '当前浏览器没有提供麦克风或 WebRTC，请使用支持的浏览器。'
    : state.voiceAvailable ? `${state.voiceModel} · 连续对话、自动回应、开口打断。麦克风仅在开启后采集。`
    : '启动 Shell 时需要实时语音模型配置和本次完整访问链接。';
}
function updateVoiceCaption(event) {
  if (!['user', 'assistant'].includes(event.role) || typeof event.text !== 'string') return;
  const id = `${event.role}:${event.itemId || event.responseId || 'current'}`;
  let caption = state.voiceCaptions.find(item => item.id === id);
  if (!caption) { caption = { id, role: event.role }; state.voiceCaptions.push(caption); }
  Object.assign(caption, { text: event.text.slice(0, 2000), final: event.final === true, interrupted: event.interrupted === true });
  state.voiceCaptions = state.voiceCaptions.slice(-12);
  $('#voice-captions').innerHTML = state.voiceCaptions.slice(-2).map(item => `<div class="voice-caption ${item.role === 'user' ? 'from-user' : 'from-assistant'}"><span>${item.role === 'user' ? '你' : 'AURA'}</span><p>${e(item.text)}${item.interrupted ? '<small>回应已打断</small>' : ''}</p></div>`).join('');
  if (event.final && event.text.trim()) {
    $('#voice-announcement').textContent = `${event.role === 'user' ? '你' : 'AURA'}：${event.text.slice(0, 300)}`;
    if (event.role === 'user') { state.lastIntent = event.text.trim().slice(0, 4000); queueLayout(); }
  }
}
async function controlVoiceFocus({ action, minutes }) {
  if (action !== 'pause' && minutes !== null && minutes !== undefined && minutes !== state.focus.duration) {
    state.focus.duration = minutes; state.focus.remaining = minutes * 60;
    state.focus.endAt = null;
  }
  if (action === 'start' && !state.focus.endAt) {
    if (state.focus.remaining <= 0) state.focus.remaining = state.focus.duration * 60;
    state.focus.endAt = Date.now() + state.focus.remaining * 1000;
    addLocalActivity('语音开启专注', `已开启 ${state.focus.duration} 分钟专注计时。`);
  } else if (action === 'pause' && state.focus.endAt) { state.focus.remaining = focusRemaining(); state.focus.endAt = null; }
  else if (action === 'reset') { state.focus.endAt = null; state.focus.remaining = state.focus.duration * 60; }
  persistFocus();
  await navigate('focus', { load: false });
  syncVoiceContext();
  return { ok: true, scene: 'focus', active: !!state.focus.endAt, duration_minutes: state.focus.duration, remaining_seconds: focusRemaining() };
}
function stopListening() {
  voiceToolRouter.reset(); lastVoiceContext = '';
  Promise.resolve(realtimeVoice.stop()).catch(() => {});
}
async function startListening() {
  if (state.voice.active || state.voice.phase === 'connecting') { stopListening(); return; }
  if (!supportsRealtimeVoice()) { toast('当前浏览器不支持实时麦克风连接，可继续使用键盘。'); return; }
  if (!state.voiceAvailable) await refreshVoiceAvailability();
  if (!state.voiceAvailable) { toast('实时语音尚未连接。请检查模型配置与完整访问链接。'); return; }
  if ('speechSynthesis' in window) speechSynthesis.cancel();
  voiceToolRouter.reset(); state.voiceCaptions = []; lastVoiceContext = '';
  $('#voice-captions').replaceChildren();
  try { await realtimeVoice.start(); }
  catch { updateVoiceControls(); }
}
async function requestVoice() {
  if (state.voice.active || state.voice.phase === 'connecting') { stopListening(); return; }
  if (!state.voiceConsent || !supportsRealtimeVoice() || !state.voiceAvailable) {
    await refreshVoiceAvailability();
    $('#voice-support').textContent = !supportsRealtimeVoice() ? '当前浏览器未提供可用的麦克风或 WebRTC。请使用支持的浏览器，或继续键盘操作。'
      : state.voiceAvailable ? '连接后可以一直说话；字幕自动显示，意图通过工具执行，无需点击提交。'
      : '尚未连接实时语音模型。请使用启动时的完整链接，并检查实时语音模型配置。';
    $('#voice-consent-button').disabled = !supportsRealtimeVoice() || !state.voiceAvailable;
    if (!$('#voice-dialog').open) $('#voice-dialog').showModal();
  } else startListening();
}

$('#intent-form').addEventListener('submit', event => { event.preventDefault(); submitIntent($('#intent-input').value); });
$('#mic-button').addEventListener('click', requestVoice);
$('#keyboard-button').addEventListener('click', () => $('#intent-input').focus());
$('#settings-button').addEventListener('click', () => $('#settings-dialog').showModal());
$('#connection-button').addEventListener('click', () => { refreshConnection(); $('#settings-dialog').showModal(); });
$('#privacy-button').addEventListener('click', () => $('#privacy-dialog').showModal());
$('#voice-consent-button').addEventListener('click', () => { state.voiceConsent = true; storage.set('realtimeVoiceConsent', true); $('#voice-dialog').close(); startListening(); });
$('#voice-interrupt').addEventListener('click', () => { realtimeVoice.interrupt(); respond('已经停下。你可以接着说。'); });
$('#voice-mute').addEventListener('click', () => realtimeVoice.setMuted(!state.voice.muted));
$('#voice-end').addEventListener('click', stopListening);
$('#speech-toggle').addEventListener('click', () => {
  state.speech = !state.speech; storage.set('speech', state.speech); $('#speech-toggle').setAttribute('aria-checked', String(state.speech));
  if (!state.speech && 'speechSynthesis' in window) speechSynthesis.cancel();
  respond(state.speech ? '语音回应已开启。' : '我会安静地把结果留在屏幕上。', state.speech);
});
$('#motion-toggle').addEventListener('click', () => {
  state.motion = !state.motion; storage.set('motion', state.motion); $('#motion-toggle').setAttribute('aria-checked', String(state.motion));
  document.body.classList.toggle('reduced-motion', !state.motion); stopOrb(); if ($('#orb-canvas')) startOrb();
});
$('#layout-toggle').addEventListener('click', () => {
  state.layoutEnabled = !state.layoutEnabled;
  storage.set('modelLayout', state.layoutEnabled);
  $('#layout-toggle').setAttribute('aria-checked', String(state.layoutEnabled));
  layoutCoordinator.reset(); state.generatedScene = null; state.generatedKey = '';
  render();
  if (state.layoutEnabled) refreshLayoutAvailability();
});
document.addEventListener('click', async event => {
  const button = event.target.closest('button'); if (!button || button.disabled) return;
  if (button.dataset.closeDialog) { document.getElementById(button.dataset.closeDialog).close(); return; }
  if (button.id === 'regenerate-layout' || button.id === 'regenerate-scene') {
    if (!state.layoutAvailable) await refreshLayoutAvailability();
    if (state.layoutEnabled && state.layoutAvailable) {
      queueLayout(); layoutCoordinator.retry();
    }
    return;
  }
  if (button.dataset.scene) { navigate(button.dataset.scene); return; }
  if (button.dataset.intent) { submitIntent(button.dataset.intent); return; }
  if (button.dataset.selectTask) { await selectTask(button.dataset.selectTask, { readApproval: true }); return; }
  if (button.id === 'new-note') { $('#intent-input').value = '记一下 '; $('#intent-input').focus(); return; }
  if (button.dataset.deleteNote) {
    state.notes = state.notes.filter(n => n.id !== button.dataset.deleteNote); saveNotes(); addLocalActivity('移除一条浏览器笔记', '笔记已从浏览器移除。'); render(); respond('这个念头已移除。'); return;
  }
  if (button.dataset.duration) {
    if (state.focus.endAt) { toast('先暂停计时，再调整时长。'); return; }
    state.focus.duration = Number(button.dataset.duration); state.focus.remaining = state.focus.duration * 60; persistFocus(); render(); return;
  }
  if (button.id === 'focus-toggle') {
    if (state.focus.endAt) { state.focus.remaining = focusRemaining(); state.focus.endAt = null; respond('暂停一下，也很好。'); }
    else { if (state.focus.remaining <= 0) state.focus.remaining = state.focus.duration * 60; state.focus.endAt = Date.now() + state.focus.remaining * 1000; addLocalActivity('开始专注', `在浏览器开启 ${state.focus.duration} 分钟专注计时。`); respond('好，把这一刻留给你。', true); }
    persistFocus(); render(); return;
  }
  if (button.id === 'focus-reset') { state.focus.endAt = null; state.focus.remaining = state.focus.duration * 60; persistFocus(); render(); respond('准备好了，随时可以重新开始。'); return; }
  if (button.id === 'refresh-tasks') { if (state.connected) { try { await refreshTasks(); } catch (error) { handleError(error); } } render(); return; }
  if (button.id === 'tasks-next' || button.id === 'tasks-previous') {
    if (button.id === 'tasks-next') { state.taskHistory.push(state.currentBefore); state.currentBefore = state.nextBefore; }
    else state.currentBefore = state.taskHistory.pop() ?? null;
    try { await refreshTasks(); render(); } catch (error) { handleError(error); } return;
  }
  if (button.id === 'refresh-approval') { await selectTask(taskId(state.selected), { readApproval: true }); return; }
  if (button.id === 'approve-task') {
    const shown = state.approval; if (!shown || shown.expires_at_ms <= Date.now() || !state.connected) { state.approval = null; render(); return; }
    button.disabled = true;
    try {
      const fresh = (await api(`/tasks/${encodeURIComponent(button.dataset.taskId)}`)).task;
      if (fresh.id !== shown.id || fresh.confirmation?.token !== shown.token || fresh.confirmation?.step !== shown.step || fresh.confirmation.expires_at_ms <= Date.now()) {
        state.selected = fresh; state.approval = null; render(); respond('批准请求已经变化，请重新读取并查看。'); return;
      }
      state.approval = null;
      await api(`/tasks/${encodeURIComponent(button.dataset.taskId)}/approve`, { step: shown.step + 1, confirmation: shown.token });
      respond('这一具体步骤的批准已入队，后台会再次校验。'); await refreshTasks(); render();
    } catch (error) { state.approval = null; handleError(error); render(); } return;
  }
  if (button.id === 'cancel-task') {
    button.disabled = true;
    try { const data = await api(`/tasks/${encodeURIComponent(button.dataset.taskId)}/cancel`, {}); state.selected = data.task; state.approval = null; await refreshTasks(); render(); respond('已请求停止后续步骤。已经开始的动作可能仍会完成。'); } catch (error) { handleError(error); render(); }
  }
});
document.addEventListener('input', event => {
  if (event.target.id === 'focus-goal') { state.focus.goal = event.target.value; persistFocus(); queueLayout(); }
});
document.addEventListener('keydown', event => {
  const editing = event.target.matches('input,textarea,[contenteditable]');
  if (event.key === 'Escape') {
    if (!$('dialog[open]')) { if (event.shiftKey) stopListening(); else realtimeVoice.interrupt(); }
    if ('speechSynthesis' in window) speechSynthesis.cancel(); return;
  }
  if (event.altKey && event.code === 'KeyV') { event.preventDefault(); if (!$('dialog[open]')) requestVoice(); return; }
  if ($('dialog[open]')) return;
  if (event.altKey && /^Digit[1-4]$/.test(event.code)) { event.preventDefault(); navigate(['overview', 'focus', 'memory', 'activity'][Number(event.code.slice(-1)) - 1]); return; }
  if (event.key === '/' && !editing && !event.metaKey && !event.ctrlKey) { event.preventDefault(); $('#intent-input').focus(); }
});
for (const dialog of document.querySelectorAll('dialog')) dialog.addEventListener('click', event => { if (event.target === dialog) { const r = dialog.getBoundingClientRect(); if (event.clientX < r.left || event.clientX > r.right || event.clientY < r.top || event.clientY > r.bottom) dialog.close(); } });
document.addEventListener('visibilitychange', () => { if (document.hidden) { stopListening(); cancelAnimationFrame(orbFrame); } else { orbRedraw?.(); refreshConnection(); tick(); } });
window.addEventListener('hashchange', () => {
  stopListening(); readSession(); layoutCoordinator.reset(); state.generatedScene = null; state.generatedKey = '';
  state.layoutAvailable = false; state.voiceAvailable = false;
  refreshConnection(); refreshLayoutAvailability(); refreshVoiceAvailability();
});
window.addEventListener('pagehide', stopListening);
window.addEventListener('resize', () => { clearTimeout(viewportTimer); viewportTimer = setTimeout(queueLayout, 300); });
document.addEventListener('focusout', () => requestAnimationFrame(renderWhenUnoccupied));
prefersReducedMotion.addEventListener('change', event => { if (event.matches) { state.motion = false; document.body.classList.add('reduced-motion'); $('#motion-toggle').setAttribute('aria-checked', 'false'); stopOrb(); if ($('#orb-canvas')) startOrb(); } });
function tick() {
  $('#system-time').textContent = formatTime();
  if ($('#big-clock')) $('#big-clock').textContent = formatTime(new Date(), true);
  if (state.focus.endAt && focusRemaining() === 0) {
    state.focus.endAt = null; state.focus.remaining = 0; persistFocus(); addLocalActivity('完成一段专注', '这一段专注计时已经结束。');
    respond('这一段专注完成了。伸伸懒腰，给自己一点休息。', true);
    if (state.scene === 'focus' || $('#focus-timer')) { canvasDirty = true; renderWhenUnoccupied(); }
    queueLayout();
  }
  if ($('#focus-timer')) $('#focus-timer').textContent = formatRemaining();
  if (state.approval && state.approval.expires_at_ms <= Date.now()) { state.approval = null; if (['activity', 'task'].includes(state.scene)) render(); }
}
document.body.classList.toggle('reduced-motion', !state.motion);
$('#speech-toggle').setAttribute('aria-checked', String(state.speech));
$('#motion-toggle').setAttribute('aria-checked', String(state.motion));
$('#layout-toggle').setAttribute('aria-checked', String(state.layoutEnabled));
setConnection(false, 'starting'); render(); tick(); refreshConnection(); refreshLayoutAvailability(); refreshVoiceAvailability();
setInterval(tick, 1000);
setInterval(refreshConnection, 2500);
