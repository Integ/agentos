/**
 * Semantic presentation, independent of the DOM and of task execution.
 *
 * A scene describes emphasis and a small set of trusted components. It never
 * contains scripts, URLs, or permission to execute a capability. Local scenes
 * provide an immediate fallback; model proposals select composition and prose,
 * while actual data is bound independently from trusted runtime facts.
 */

const KINDS = new Set(['overview', 'focus', 'memory', 'activity', 'clock', 'help', 'note', 'task']);
const LAYOUTS = new Set(['ambient', 'split', 'focused', 'stack']);
const PANEL_TYPES = new Set(['welcome', 'insight', 'suggestions', 'notes', 'task', 'activity', 'clock', 'focus', 'capabilities']);
const TASK_STATES = new Set(['queued', 'planning', 'running', 'awaiting_confirmation', 'succeeded', 'failed', 'cancelled', 'interrupted', 'unknown']);
const STEP_STATES = new Set(['pending', 'running', 'succeeded', 'failed', 'unknown']);
const SCENE_KEYS = new Set(['kind', 'eyebrow', 'title', 'description', 'layout', 'panels']);
const PANEL_KEYS = new Set(['type', 'priority', 'span', 'title', 'description', 'items', 'task', 'active', 'duration', 'unix_ms']);
const MODEL_PANEL_KEYS = new Set(['type', 'priority', 'span', 'title', 'description']);

const isObject = value => value !== null && typeof value === 'object' && !Array.isArray(value)
  && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
const text = (value, limit = 500) => typeof value === 'string' ? value.slice(0, limit) : '';
const knownKeys = (value, allowed) => Object.keys(value).every(key => allowed.has(key));
const safeInteger = value => Number.isSafeInteger(value) && value >= 0;

/** Local navigation only. Any unrecognized sentence remains an actual task. */
export function classifyIntent(value) {
  const original = typeof value === 'string' ? value.trim() : '';
  if (!original) return { kind: 'overview' };
  // Prefixes are anchored: questions, quotations, and negations must not be
  // converted into writes by keyword matching. Preserve the note's exact text.
  const chineseNote = /^(?:记一下|记住|记个笔记)([\s\S]*)$/u.exec(original);
  const englishNote = /^note(?:\s+|\s*[:：]\s*)([\s\S]+)$/iu.exec(original);
  const note = chineseNote ? chineseNote[1].replace(/^[\s:：]+/u, '') : englishNote?.[1];
  if (note?.trim()) return { kind: 'note', note: note.trim() };

  const normalized = original.toLowerCase().replace(/[。.!！?？]+$/u, '').trim();
  const command = normalized.replace(/^(?:请帮我|帮我|请|打开|显示|切换到)\s*/u, '').trim();
  if (/^(?:规划今天|今天有什么|早间简报|今日概览|回到首页|首页|总览|overview|home|briefing|plan my day)$/u.test(command)) return { kind: 'overview' };
  if (/^(?:开始)?(?:专注|专注模式|勿扰|focus|focus mode)(?:\s*\d+\s*(?:分钟|minutes?|min))?$/u.test(command)) return { kind: 'focus' };
  if (/^(?:看看笔记|查看笔记|列出笔记|列出所有笔记|整理灵感|我的灵感|记忆|记忆空间|笔记|笔记本|notes|list notes|show notes|memory)$/u.test(command)) return { kind: 'memory' };
  if (/^(?:任务|我的任务|任务进展|执行记录|进展|任务历史|活动|tasks|activity|history|\/tasks)$/u.test(command)) return { kind: 'activity' };
  if (/^(?:时间|几点|几点了|现在几点|现在几点了|现在时间|当前时间|time|what time is it|clock)$/u.test(command)) return { kind: 'clock' };
  if (/^(?:帮助|你能做什么|有什么能力|能力|help|capabilities|\/help)$/u.test(command)) return { kind: 'help' };
  return { kind: 'task' };
}

function cloneData(value, maximum = 65536) {
  if (value === undefined) return null;
  try {
    const encoded = JSON.stringify(value);
    if (encoded === undefined || encoded.length > maximum) return null;
    return JSON.parse(encoded);
  } catch {
    return null;
  }
}

/** Accept both daemon task summaries and full tasks; never invent progress. */
export function normalizeTask(value) {
  if (!isObject(value)) return null;
  const id = typeof value.id === 'string' && /^task-\d+$/u.test(value.id)
    ? Number(value.id.slice(5)) : value.id;
  if (!safeInteger(id)) return null;
  const steps = Array.isArray(value.steps) ? value.steps.slice(0, 100).filter(isObject).map(step => ({
    capability: text(step.capability, 160),
    state: STEP_STATES.has(step.state) ? step.state : 'unknown',
    args: cloneData(step.args),
    result: cloneData(step.result),
  })) : [];
  const confirmation = isObject(value.confirmation)
    && safeInteger(value.confirmation.step)
    && safeInteger(value.confirmation.expires_at_ms)
    && typeof value.confirmation.token === 'string'
    && value.confirmation.token.length <= 4096
    ? { step: value.confirmation.step, expires_at_ms: value.confirmation.expires_at_ms, token: value.confirmation.token } : null;
  return {
    id,
    text: text(value.intent?.text ?? value.text, 65536),
    state: TASK_STATES.has(value.state) ? value.state : 'unknown',
    message: typeof value.message === 'string' ? text(value.message, 65536) : null,
    steps,
    confirmation,
  };
}

function readNotes(notes, task) {
  if (Array.isArray(notes)) return notes.filter(note => typeof note === 'string').slice(0, 100).map(note => text(note, 16384));
  const step = task?.steps.findLast(step => step.capability === 'notes.list'
    && step.state === 'succeeded' && Array.isArray(step.result?.notes));
  return step ? readNotes(step.result.notes, null) : [];
}

function overview() {
  return {
    kind: 'overview',
    eyebrow: '此刻 · 意图空间',
    title: '从一个念头开始。',
    description: '说出你想做的事，工作空间会随情景展开。',
    layout: 'ambient',
    panels: [
      { type: 'welcome', priority: 1, title: '你的意图，就是入口。', description: '记下灵感、回顾任务，或者留一点时间给专注。' },
      { type: 'suggestions', priority: 2, items: ['记一下 明天去散步', '看看笔记', '开始专注', '现在几点'] },
      { type: 'activity', priority: 3, title: '最近的执行' },
    ],
  };
}

/** Compose only from runtime facts and local presentation state. */
export function composeScene({ kind = 'overview', task: rawTask, notes, focus } = {}) {
  if (!KINDS.has(kind) || kind === 'overview') return overview();
  const task = normalizeTask(rawTask);
  const items = readNotes(notes, task);
  if (kind === 'focus') return {
    kind, eyebrow: '留白 · 专注空间', title: '把注意力，留给一件事。',
    description: '启动一个本地专注计时，或随时回到工作空间。', layout: 'focused',
    panels: [{ type: 'focus', priority: 1, active: focus === true || focus?.active === true,
      duration: Number.isFinite(focus?.duration) ? Math.min(14400, Math.max(60, Math.round(focus.duration))) : 1500 }],
  };
  if (kind === 'memory') return {
    kind, eyebrow: '记忆 · 灵感空间', title: '让想法，有处安放。',
    description: '说“记一下”，留下一个念头。说“看看笔记”，回到你的记录。', layout: 'split',
    panels: [{ type: 'notes', priority: 1, title: '我的笔记', items },
      { type: 'suggestions', priority: 2, items: ['记一下 一个新的想法', '看看笔记', '恢复笔记'] }],
  };
  if (kind === 'activity') return {
    kind, eyebrow: '轨迹 · 执行空间', title: '每一步，都清晰可见。',
    description: '任务由后台服务持续执行。选择一项，查看它的真实状态与结果。', layout: 'stack',
    panels: [{ type: 'activity', priority: 1, title: '执行记录' },
      ...(task ? [{ type: 'task', priority: 2, task }] : [])],
  };
  if (kind === 'clock') {
    const clock = task?.steps.findLast(step => step.capability === 'clock.now'
      && step.state === 'succeeded' && safeInteger(step.result?.unix_ms));
    return {
      kind, eyebrow: '此刻 · 时间空间', title: '此时，此地。',
      description: '给自己一点时间，感受当下。', layout: 'focused',
      panels: [{ type: 'clock', priority: 1, ...(clock ? { unix_ms: clock.result.unix_ms } : {}) }],
    };
  }
  if (kind === 'help') return {
    kind, eyebrow: '连接 · 能力空间', title: '从你的一句话开始。',
    description: '可用能力以后台服务的实际授权为准。', layout: 'stack',
    panels: [{ type: 'capabilities', priority: 1, title: '当前能力' },
      { type: 'suggestions', priority: 2, items: ['记一下 买牛奶', '看看笔记', '清空笔记', '恢复笔记', '现在几点'] }],
  };
  return {
    kind, eyebrow: '意图 · 执行空间',
    title: kind === 'note' ? '接住这个念头。' : '让意图，变成行动。',
    description: task ? task.text.slice(0, 500) : '准备好以后，说出你想做的事。',
    layout: 'stack',
    panels: task ? [{ type: 'task', priority: 1, task },
      ...(items.length ? [{ type: 'notes', priority: 2, title: '笔记', items }] : [])]
      : [{ type: 'suggestions', priority: 1, items: ['记一下 买牛奶', '看看笔记', '现在几点'] }],
  };
}

/**
 * Validate a render scene after factual binding. Local invalid scenes fall back
 * as a whole. Model proposals use validateModelScene instead, so a rejected
 * proposal cannot be mistaken for successful model generation.
 * Components must render strings as text; markup is never interpreted here.
 */
export function validateScene(input) {
  if (!isObject(input) || !knownKeys(input, SCENE_KEYS)
    || !KINDS.has(input.kind) || !LAYOUTS.has(input.layout)
    || typeof input.eyebrow !== 'string' || input.eyebrow.length > 80
    || typeof input.title !== 'string' || !input.title.trim() || input.title.length > 160
    || typeof input.description !== 'string' || input.description.length > 500
    || !Array.isArray(input.panels) || input.panels.length < 1 || input.panels.length > 8) return overview();

  for (const panel of input.panels) {
    if (!isObject(panel) || !knownKeys(panel, PANEL_KEYS) || !PANEL_TYPES.has(panel.type)
      || !Number.isInteger(panel.priority) || panel.priority < 1 || panel.priority > 8
      || (panel.span !== undefined && panel.span !== 1 && panel.span !== 2)
      || (panel.title !== undefined && (typeof panel.title !== 'string' || panel.title.length > 160))
      || (panel.description !== undefined && (typeof panel.description !== 'string' || panel.description.length > 500))
      || (panel.items !== undefined && (!Array.isArray(panel.items) || panel.items.length > 100
        || panel.items.some(item => typeof item !== 'string' || item.length > 16384)))
      || (panel.active !== undefined && typeof panel.active !== 'boolean')
      || (panel.duration !== undefined && (!safeInteger(panel.duration) || panel.duration < 60 || panel.duration > 14400))
      || (panel.unix_ms !== undefined && !safeInteger(panel.unix_ms))
      || (panel.task !== undefined && !normalizeTask(panel.task))) return overview();
  }
  return {
    kind: input.kind, eyebrow: input.eyebrow, title: input.title, description: input.description, layout: input.layout,
    panels: input.panels.map(panel => ({ ...panel, ...(panel.items ? { items: [...panel.items] } : {}),
      ...(panel.task ? { task: normalizeTask(panel.task) } : {}) })),
  };
}

/**
 * Model output controls presentation only. Requiring every property and
 * rejecting every unknown key excludes forged task data, action arguments,
 * confirmation tokens, markup handlers and model-provided component code.
 * Text values are still untrusted and must be escaped by the DOM renderer.
 */
export function validateModelScene(input, expectedKind) {
  if (!isObject(input) || !knownKeys(input, SCENE_KEYS)
    || Object.keys(input).length !== SCENE_KEYS.size
    || !KINDS.has(input.kind) || (expectedKind !== undefined && input.kind !== expectedKind)
    || !LAYOUTS.has(input.layout)
    || typeof input.eyebrow !== 'string' || input.eyebrow.length > 80
    || typeof input.title !== 'string' || !input.title.trim() || input.title.length > 160
    || typeof input.description !== 'string' || input.description.length > 500
    || !Array.isArray(input.panels) || input.panels.length < 1 || input.panels.length > 8) return null;

  const priorities = new Set();
  const types = new Set();
  for (const panel of input.panels) {
    if (!isObject(panel) || !knownKeys(panel, MODEL_PANEL_KEYS)
      || Object.keys(panel).length !== MODEL_PANEL_KEYS.size
      || !PANEL_TYPES.has(panel.type) || types.has(panel.type)
      || !Number.isInteger(panel.priority) || panel.priority < 1 || panel.priority > 8
      || priorities.has(panel.priority) || (panel.span !== 1 && panel.span !== 2)
      || typeof panel.title !== 'string' || panel.title.length > 160
      || typeof panel.description !== 'string' || panel.description.length > 500) return null;
    priorities.add(panel.priority);
    types.add(panel.type);
  }
  return {
    kind: input.kind, eyebrow: input.eyebrow, title: input.title,
    description: input.description, layout: input.layout,
    panels: input.panels.map(panel => ({
      type: panel.type, priority: panel.priority, span: panel.span,
      title: panel.title, description: panel.description,
    })),
  };
}

/**
 * Attach only caller-owned facts to a model's component plan. A task shown by
 * this function is the selected runtime task, never a task supplied by a model.
 * Approval controls must additionally use the latest daemon record; even a
 * correctly bound render scene is not an authorization boundary.
 */
export function bindModelScene(proposal, facts = {}) {
  if (!isObject(facts)) return null;
  const scene = validateModelScene(proposal, facts.kind);
  if (!scene) return null;
  const task = normalizeTask(facts.task);
  const notes = readNotes(facts.notes, task);
  const local = composeScene({ kind: scene.kind, task, notes, focus: facts.focus });
  const localSuggestions = local.panels.find(panel => panel.type === 'suggestions')
    || overview().panels.find(panel => panel.type === 'suggestions');
  const clock = task?.steps.findLast(step => step.capability === 'clock.now'
    && step.state === 'succeeded' && safeInteger(step.result?.unix_ms));
  function bindPanel(panel) {
    if (panel.type === 'notes') return { ...panel, items: [...notes] };
    if (panel.type === 'task') return { ...panel, ...(task ? { task } : {}) };
    if (panel.type === 'suggestions') return { ...panel, items: [...localSuggestions.items] };
    if (panel.type === 'clock') return { ...panel, ...(clock ? { unix_ms: clock.result.unix_ms } : {}) };
    if (panel.type === 'focus') return {
      ...panel,
      active: facts.focus === true || facts.focus?.active === true,
      duration: Number.isFinite(facts.focus?.duration)
        ? Math.min(14400, Math.max(60, Math.round(facts.focus.duration))) : 1500,
    };
    if (panel.type === 'capabilities' && Array.isArray(facts.capabilities)) return {
      ...panel,
      items: facts.capabilities.slice(0, 100).filter(isObject)
        .map(capability => `${text(capability.name, 160)}${capability.description ? ` · ${text(capability.description, 500)}` : ''}`),
    };
    return { ...panel };
  }
  let panels = scene.panels.map(bindPanel).sort((left, right) => left.priority - right.priority);

  // A generated view may choose its supporting content freely, but navigating
  // into a named space must still expose that space's primary controls.
  const primaryTypes = { focus: 'focus', memory: 'notes', activity: 'activity', clock: 'clock', help: 'capabilities' };
  const primaryType = primaryTypes[scene.kind] || (task && ['task', 'note'].includes(scene.kind) ? 'task' : null);
  const primaryTitles = { focus: '专注计时', notes: '我的笔记', activity: '执行记录', clock: '此刻时间', capabilities: '当前能力', task: '当前任务' };
  if (primaryType && !panels.some(panel => panel.type === primaryType)) {
    const primary = bindPanel({ type: primaryType, priority: 1, span: 2,
      title: primaryTitles[primaryType], description: '' });
    panels = [primary, ...panels].slice(0, 8)
      .map((panel, index) => ({ ...panel, priority: index + 1 }));
  }

  const confirmation = task?.state === 'awaiting_confirmation';
  const requiresTask = task && confirmation;
  if (requiresTask) {
    const existing = panels.find(panel => panel.type === 'task');
    if (!existing || confirmation) {
      // A model must not hide confirmation behind another panel, including when
      // all eight slots have been used. Keep the remaining relative ordering.
      const required = existing || {
        type: 'task', priority: 1, span: 2,
        title: confirmation ? '等待你的明确批准' : '当前任务', description: '', task,
      };
      panels = [required, ...panels.filter(panel => panel.type !== 'task')].slice(0, 8)
        .map((panel, index) => ({ ...panel, priority: index + 1 }));
    }
  }
  return validateScene({ ...scene, panels });
}
