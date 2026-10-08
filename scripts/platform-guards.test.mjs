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

test('repeated launches reuse the main window before account state and services start', () => {
  const manifest = fs.readFileSync('src-tauri/Cargo.toml', 'utf8');
  assert.match(manifest, /tauri-plugin-single-instance\s*=\s*\{[^}]*features\s*=\s*\["deep-link"\]/);
  const source = fs.readFileSync('src-tauri/src/lib.rs', 'utf8');
  const run = source.slice(source.indexOf('pub fn run()'));
  assert.ok(run.indexOf('.plugin(tauri_plugin_single_instance::init(') < run.indexOf('.plugin(tauri_plugin_opener::init())'));
  assert.match(run, /tauri_plugin_single_instance::init\(\|app, _args, _cwd\|\s*\{\s*crate::tray::show_main_window_from_cmd\(app\);\s*\}/);
  assert.ok(run.indexOf('app.manage(AppState::new());') > run.indexOf('.setup(|app|'));
  assert.ok(!run.slice(0, run.indexOf('.setup(|app|')).includes('AppState::new()'));
  assert.ok(run.includes('app.deep_link().on_open_url'));
  assert.ok(run.includes('api.prevent_close();'));
});
