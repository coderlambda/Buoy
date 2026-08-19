import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { MobileDomInputReconciler } from '../ui/src/terminalTab.js';

function applyTerminalInput(current: string, input: string): string {
  let result = current;
  for (const character of Array.from(input)) {
    result = character === '\x7f'
      ? Array.from(result).slice(0, -1).join('')
      : result + character;
  }
  return result;
}

test('TC-MI1 reconciles streaming iOS dictation snapshots and corrections', () => {
  const reconciler = new MobileDomInputReconciler();
  const snapshots = [
    'summarize',
    'summarize the content',
    'summarize the counting',
    'summarize the counting current',
    'summarize the counting current package',
  ];
  let rendered = '';
  let textarea = '';
  snapshots.forEach((snapshot) => {
    const edit = reconciler.reconcile(textarea, snapshot);
    rendered = applyTerminalInput(rendered, edit.output);
    textarea = snapshot;
  });
  assert.equal(rendered, snapshots.at(-1));
});

test('TC-MI2 preserves true appended input during and outside a snapshot stream', () => {
  const reconciler = new MobileDomInputReconciler();
  let rendered = '';
  rendered = applyTerminalInput(rendered, reconciler.reconcile('', 'hello').output);
  rendered = applyTerminalInput(rendered, reconciler.reconcile('hello', 'hello world').output);
  rendered = applyTerminalInput(rendered, reconciler.reconcile('hello world', 'hello world!').output);
  assert.equal(rendered, 'hello world!');
});

test('TC-MI3 replaces the entire changed tail without guessing a candidate timeout', () => {
  const reconciler = new MobileDomInputReconciler();
  const edit = reconciler.reconcile('summarize the content', 'summarize the counting');
  assert.ok(edit.deleteCount > 0);
  assert.ok(edit.insertCount > 0);
  assert.equal(applyTerminalInput('summarize the content', edit.output), 'summarize the counting');
});
