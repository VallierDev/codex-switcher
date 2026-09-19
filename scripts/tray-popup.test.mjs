import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';

test('tray grants only event subscriptions and relies on native main-window command to hide', () => {
  const cap = JSON.parse(fs.readFileSync('src-tauri/capabilities/tray.json', 'utf8'));
  assert.deepEqual(cap.windows, ['tray-popup']);
  assert.deepEqual(cap.permissions, ['core:event:allow-listen', 'core:event:allow-unlisten']);
  const source = fs.readFileSync('src/components/TrayPopup.tsx', 'utf8');
  assert.ok(source.includes("invoke('show_main_window_cmd')"));
  assert.ok(!source.includes('getCurrentWebviewWindow().hide'));
});

test('small work-area popup retains vertical access to action buttons', () => {
  const css = fs.readFileSync('src/components/TrayPopup.css', 'utf8');
  assert.match(css, /\.tray-popup\s*\{[^}]*overflow-y:\s*auto/s);
  assert.match(css, /\.tp-actions\s*\{[^}]*flex-shrink:\s*0/s);
});
