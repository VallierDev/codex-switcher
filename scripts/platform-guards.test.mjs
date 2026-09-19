import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

test('only Mac platforms expose AppleScript actions', () => {
  const source = fs.readFileSync('src/platform.ts', 'utf8').replace('export const isMacOS', 'globalThis.isMacOS');
  for (const [platform, expected] of [['Win32', false], ['Linux x86_64', false], ['MacIntel', true], ['MacARM', true]]) {
    const context = { navigator: { platform } };
    vm.runInNewContext(source, context);
    assert.equal(context.isMacOS, expected);
  }
  const context = {};
  vm.runInNewContext(source, context);
  assert.equal(context.isMacOS, false);
});

test('both app reload paths and the toolbar gate saved settings by platform', () => {
  const app = fs.readFileSync('src/App.tsx', 'utf8');
  assert.equal(app.match(/if \(isMacOS && settings.auto_reload_ide\)/g)?.length, 2);
  const list = fs.readFileSync('src/components/AccountList.tsx', 'utf8');
  assert.ok(list.includes('disabled={!isMacOS}'));
  assert.ok(list.includes('aria-pressed={isMacOS && autoReload}'));
});
