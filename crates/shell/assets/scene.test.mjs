import test from 'node:test';
import assert from 'node:assert/strict';
import { bindModelScene, classifyIntent, composeScene, normalizeTask, validateModelScene, validateScene } from './scene.js';

test('local navigation recognizes supported Chinese and English scenes', () => {
  for (const [phrases, kind] of [
    [['规划今天', '今天有什么', '早间简报', 'overview'], 'overview'],
    [['专注', '开始专注', '勿扰', 'focus'], 'focus'],
    [['看看笔记', '整理灵感', '记忆', '笔记本', 'notes'], 'memory'],
    [['任务', '执行记录', '进展', 'tasks'], 'activity'],
    [['现在几点', 'what time is it?', 'clock'], 'clock'],
    [['帮助', '你能做什么', 'help'], 'help'],
  ]) for (const phrase of phrases) assert.equal(classifyIntent(phrase).kind, kind, phrase);
});

test('note prefixes preserve user content and do not match embedded commands', () => {
  assert.deepEqual(classifyIntent('记一下：明天 10:30 看“2001”'), { kind: 'note', note: '明天 10:30 看“2001”' });
  assert.deepEqual(classifyIntent('note  Keep My CASE!'), { kind: 'note', note: 'Keep My CASE!' });
  assert.deepEqual(classifyIntent('记住不要清空笔记'), { kind: 'note', note: '不要清空笔记' });
  for (const phrase of ['不要记一下 买牛奶', '怎么记住明天的事', '“记一下 买牛奶”', 'notebook', '记一下：', '清空笔记', '不要清空笔记', '恢复笔记']) {
    assert.equal(classifyIntent(phrase).kind, 'task', phrase);
  }
});

test('composition uses successful daemon results without fabricating notes or time', () => {
  const task = { id: 12, intent: { text: '看看笔记' }, state: 'succeeded', steps: [
    { capability: 'notes.list', state: 'succeeded', args: {}, result: { notes: ['一条真实笔记'] } },
  ] };
  const scene = composeScene({ kind: 'memory', task });
  assert.deepEqual(scene.panels[0].items, ['一条真实笔记']);
  task.steps[0].state = 'failed';
  assert.deepEqual(composeScene({ kind: 'memory', task }).panels[0].items, []);
  assert.equal(composeScene({ kind: 'clock' }).panels[0].unix_ms, undefined);
  assert.deepEqual(composeScene({ kind: 'memory' }).panels[0].items, []);
});

test('normalization respects daemon status, step results, and confirmation index', () => {
  const confirmation = { step: 0, expires_at_ms: 1794300000000, token: 'opaque' };
  const task = normalizeTask({ id: 'task-3', text: '执行', state: 'awaiting_confirmation', confirmation,
    steps: [{ capability: 'notes.clear', state: 'pending', args: {}, result: null }] });
  assert.equal(task.id, 3);
  assert.equal(task.state, 'awaiting_confirmation');
  assert.deepEqual(task.confirmation, confirmation);
  assert.equal(normalizeTask({ id: 1, state: 'invented' }).state, 'unknown');
  assert.equal(normalizeTask({ id: -1 }), null);
});

test('all built-in scenes pass schema validation and are detached copies', () => {
  for (const kind of ['overview', 'focus', 'memory', 'activity', 'clock', 'help', 'note', 'task']) {
    const scene = composeScene({ kind, notes: ['hello'] });
    assert.deepEqual(validateScene(scene), scene, kind);
    assert.notEqual(validateScene(scene), scene);
  }
});

test('scene validation rejects executable proposals, unknown components, and unbounded data', () => {
  const good = composeScene({ kind: 'memory', notes: ['safe'] });
  const bad = [
    { ...good, html: '<script>alert(1)</script>' },
    { ...good, layout: 'javascript:alert(1)' },
    { ...good, kind: 'payment' },
    { ...good, panels: [{ type: 'iframe', priority: 1, src: 'https://evil.invalid' }] },
    { ...good, panels: [{ type: 'notes', priority: 1, onclick: 'steal()' }] },
    { ...good, panels: [{ type: 'notes', priority: 1, items: Array(101).fill('x') }] },
    { ...good, panels: [{ type: 'focus', priority: 1, duration: Infinity }] },
    { ...good, title: 'x'.repeat(161) },
    JSON.parse('{"kind":"memory","layout":"split","eyebrow":"a","title":"a","description":"","panels":[],"__proto__":{"polluted":true}}'),
  ];
  for (const proposal of bad) assert.equal(validateScene(proposal).kind, 'overview');
  assert.equal({}.polluted, undefined);
  assert.equal(validateScene(Object.create({ kind: 'memory' })).kind, 'overview');
});

test('valid display strings remain literal data rather than interpreted markup', () => {
  const scene = composeScene({ kind: 'memory', notes: ['<img src=x onerror=alert(1)>'] });
  assert.equal(validateScene(scene).panels[0].items[0], '<img src=x onerror=alert(1)>');
});

function modelScene(kind = 'memory', layout = 'split', types = ['notes', 'insight']) {
  return {
    kind, layout, eyebrow: '根据当前情景', title: '让重要的事，来到眼前。',
    description: '把已有信息组织成适合此刻的空间。',
    panels: types.map((type, index) => ({ type, priority: index + 1, span: index === 0 ? 2 : 1,
      title: `${type} 面板`, description: type === 'insight' ? '模型建议：先整理一个念头。' : '' })),
  };
}

test('model scene validation rejects forged facts, executable fields and incomplete or ambiguous plans', () => {
  const good = modelScene();
  assert.deepEqual(validateModelScene(good, 'memory'), good);
  assert.notEqual(validateModelScene(good, 'memory').panels[0], good.panels[0]);
  const missingTitle = { ...good }; delete missingTitle.title;
  const missingSpan = { ...good.panels[0] }; delete missingSpan.span;
  const bad = [
    missingTitle,
    { ...good, kind: 'task' },
    { ...good, layout: 'custom-html' },
    { ...good, html: '<script>execute()</script>' },
    { ...good, confirmation: { token: 'forged' } },
    { ...good, panels: [missingSpan] },
    { ...good, panels: [{ ...good.panels[0], type: 'iframe' }] },
    { ...good, panels: [{ ...good.panels[0], items: ['invented notebook contents'] }] },
    { ...good, panels: [{ ...good.panels[0], task: { id: 9, state: 'succeeded' } }] },
    { ...good, panels: [{ ...good.panels[0], onclick: 'execute()' }] },
    { ...good, panels: [{ ...good.panels[0], span: 12 }] },
    { ...good, panels: [{ ...good.panels[0], priority: 0 }] },
    { ...good, panels: [{ ...good.panels[0], description: 'x'.repeat(501) }] },
    { ...good, panels: [{ ...good.panels[0] }, { ...good.panels[1], priority: 1 }] },
    { ...good, panels: [{ ...good.panels[0] }, { ...good.panels[0], priority: 2 }] },
    { ...good, panels: [] },
    { ...good, panels: Array(9).fill(good.panels[0]) },
    Object.assign(Object.create({ html: 'untrusted prototype' }), good),
  ];
  for (const scene of bad) assert.equal(validateModelScene(scene, 'memory'), null);
  assert.equal(bindModelScene({ ...good, actions: ['approve'] }, { kind: 'memory' }), null);
});

test('the same facts retain genuinely different model component orders, spans and layouts', () => {
  const facts = { kind: 'memory', notes: ['唯一真实的笔记'], focus: { active: false, duration: 1500 } };
  const first = modelScene('memory', 'split', ['notes', 'insight', 'suggestions']);
  const second = modelScene('memory', 'focused', ['insight', 'clock', 'notes']);
  second.title = '先看一个值得继续的念头。';
  second.panels[2].span = 2;
  const a = bindModelScene(first, facts);
  const b = bindModelScene(second, facts);
  assert.equal(a.layout, 'split');
  assert.equal(b.layout, 'focused');
  assert.deepEqual(a.panels.map(panel => panel.type), ['notes', 'insight', 'suggestions']);
  assert.deepEqual(b.panels.map(panel => panel.type), ['insight', 'clock', 'notes']);
  assert.equal(b.panels[2].span, 2);
  assert.equal(b.title, second.title);
  assert.deepEqual(a.panels[0].items, ['唯一真实的笔记']);
  assert.deepEqual(b.panels[2].items, ['唯一真实的笔记']);
  assert.deepEqual(facts.notes, ['唯一真实的笔记']);
  const unsorted = modelScene('memory', 'ambient', ['notes', 'insight', 'focus']);
  [unsorted.panels[0].priority, unsorted.panels[1].priority, unsorted.panels[2].priority] = [7, 2, 5];
  const sorted = bindModelScene(unsorted, facts);
  assert.deepEqual(sorted.panels.map(panel => panel.type), ['insight', 'focus', 'notes']);
  assert.deepEqual(sorted.panels.map(panel => panel.priority), [2, 5, 7]);
});

test('model components bind only trusted notes, runtime task results, clock and focus facts', () => {
  const task = { id: 21, intent: { text: '看看笔记' }, state: 'succeeded', steps: [
    { capability: 'notes.list', state: 'succeeded', args: {}, result: { notes: ['服务端真实记录'] } },
    { capability: 'clock.now', state: 'succeeded', args: {}, result: { unix_ms: 1794300000000 } },
  ] };
  const facts = { kind: 'activity', task, focus: { active: true, duration: 2700 },
    capabilities: [{ name: 'notes.list', description: '读取真实笔记' }] };
  const scene = bindModelScene(modelScene('activity', 'ambient', ['task', 'notes', 'focus', 'clock', 'capabilities', 'suggestions']), facts);
  const panel = type => scene.panels.find(candidate => candidate.type === type);
  assert.equal(panel('task').task.id, 21);
  assert.equal(panel('task').task.state, 'succeeded');
  assert.deepEqual(panel('notes').items, ['服务端真实记录']);
  assert.equal(panel('focus').active, true);
  assert.equal(panel('focus').duration, 2700);
  assert.equal(panel('clock').unix_ms, 1794300000000);
  assert.deepEqual(panel('capabilities').items, ['notes.list · 读取真实笔记']);
  assert.deepEqual(panel('suggestions').items, composeScene({ kind: 'overview' }).panels[1].items);
  panel('task').task.steps[0].result.notes.push('改动只在渲染副本里');
  assert.deepEqual(task.steps[0].result.notes, ['服务端真实记录']);
  // An explicitly supplied notebook snapshot wins over an old selected task.
  assert.deepEqual(bindModelScene(modelScene(), { kind: 'memory', task, notes: [] }).panels[0].items, []);
});

test('model selection cannot hide or demote an authoritative confirmation request', () => {
  const task = { id: 5, intent: { text: '需要批准的任务' }, state: 'awaiting_confirmation',
    confirmation: { step: 0, expires_at_ms: 1794300000000, token: 'trusted-daemon-token' },
    steps: [{ capability: 'notes.clear', state: 'pending', args: {}, result: null }] };
  const eightOtherTypes = ['welcome', 'insight', 'suggestions', 'notes', 'activity', 'clock', 'focus', 'capabilities'];
  const full = bindModelScene(modelScene('overview', 'ambient', eightOtherTypes), { kind: 'overview', task });
  assert.equal(full.panels.length, 8);
  assert.equal(full.panels[0].type, 'task');
  assert.equal(full.panels[0].priority, 1);
  assert.equal(full.panels[0].task.confirmation.token, 'trusted-daemon-token');
  assert.equal(full.panels.filter(panel => panel.type === 'task').length, 1);
  const demoted = modelScene('task', 'split', ['insight', 'notes', 'task']);
  const promoted = bindModelScene(demoted, { kind: 'task', task });
  assert.deepEqual(promoted.panels.map(panel => panel.type), ['task', 'insight', 'notes']);
  assert.deepEqual(promoted.panels.map(panel => panel.priority), [1, 2, 3]);
  assert.equal(task.confirmation.step, 0);
});

test('a selected task remains available when a model proposes only commentary', () => {
  const task = { id: 6, text: '不能隐藏失败', state: 'failed', message: '真实错误', steps: [] };
  const scene = bindModelScene(modelScene('task', 'stack', ['insight']), { kind: 'task', task });
  assert.deepEqual(scene.panels.map(panel => panel.type), ['task', 'insight']);
  assert.equal(scene.panels[0].task.message, '真实错误');
  assert.equal(scene.kind, 'task');
  const literal = modelScene('memory', 'stack', ['insight']);
  literal.panels[0].description = '<img src=x onerror=execute()> 只是文字';
  assert.equal(bindModelScene(literal, { kind: 'memory' }).panels.find(panel => panel.type === 'insight').description, literal.panels[0].description);
});

test('a named space always retains its primary controls despite a commentary-only model plan', () => {
  for (const [kind, primary] of [['focus', 'focus'], ['memory', 'notes'], ['activity', 'activity'], ['clock', 'clock'], ['help', 'capabilities']]) {
    const scene = bindModelScene(modelScene(kind, 'split', ['insight', 'welcome']),
      { kind, notes: ['真实笔记'], focus: { active: true, duration: 2700 } });
    assert.equal(scene.layout, 'split');
    assert.deepEqual(scene.panels.map(panel => panel.type), [primary, 'insight', 'welcome']);
    assert.equal(scene.panels[0].span, 2);
    assert.deepEqual(scene.panels.map(panel => panel.priority), [1, 2, 3]);
    if (primary === 'notes') assert.deepEqual(scene.panels[0].items, ['真实笔记']);
    if (primary === 'focus') assert.equal(scene.panels[0].duration, 2700);
  }
  const explicit = modelScene('focus', 'ambient', ['insight', 'focus']);
  assert.deepEqual(bindModelScene(explicit, { kind: 'focus' }).panels.map(panel => panel.type), ['insight', 'focus']);
});

test('confirmation promotion keeps both task approval and the current space primary controls within eight slots', () => {
  const task = { id: 7, text: '等待批准', state: 'awaiting_confirmation',
    confirmation: { step: 0, expires_at_ms: 1794300000000, token: 'real-token' }, steps: [] };
  // The model fills every slot except focus, and leaves its task pane last.
  const proposal = modelScene('focus', 'ambient', ['welcome', 'insight', 'suggestions', 'notes', 'activity', 'clock', 'capabilities', 'task']);
  const scene = bindModelScene(proposal, { kind: 'focus', task, focus: { active: false, duration: 1500 } });
  assert.equal(scene.panels.length, 8);
  assert.deepEqual(scene.panels.slice(0, 2).map(panel => panel.type), ['task', 'focus']);
  assert.equal(scene.panels[0].task.confirmation.token, 'real-token');
  assert.equal(new Set(scene.panels.map(panel => panel.type)).size, 8);
  assert.deepEqual(scene.panels.map(panel => panel.priority), [1, 2, 3, 4, 5, 6, 7, 8]);
});
