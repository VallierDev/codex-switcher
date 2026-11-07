import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';

test('Linux autostart uses XDG without account settings or distro paths', () => {
  const startup = fs.readFileSync('src-tauri/src/startup.rs', 'utf8');
  assert.ok(startup.includes('std::env::var_os("XDG_CONFIG_HOME")'));
  assert.ok(startup.includes('app.env().appimage'));
  assert.ok(startup.includes('xdg::set_enabled('));
  assert.ok(startup.includes('xdg::is_enabled('));
  assert.ok(startup.includes('if cfg!(debug_assertions)'));
  assert.ok(!/AccountStore|auth\.json|\/usr\/bin|systemctl|Ubuntu/.test(startup));
});

test('autostart hides the initial window before construction and retains tray-failure recovery', () => {
  const source = fs.readFileSync('src-tauri/src/lib.rs', 'utf8');
  const run = source.slice(source.indexOf('pub fn run()'));
  assert.ok(run.indexOf('startup::configure_initial_window(&mut context)') < run.indexOf('tauri::Builder::default()'));
  assert.ok(run.includes('.build(context)'));
  const trayFailure = run.slice(run.indexOf('if let Err(e) = tray::init'), run.indexOf('let state = app.state::<AppState>()'));
  assert.ok(trayFailure.includes('show_main_window_from_cmd(app.handle())'));
  const startup = fs.readFileSync('src-tauri/src/startup.rs', 'utf8');
  assert.ok(startup.includes('window.visible = false'));
  assert.ok(startup.includes('if window.label == "main"'));
});

test('autostart settings are saved immediately and unsupported builds do not expose the toggle', () => {
  const settings = fs.readFileSync('src/components/Settings.tsx', 'utf8');
  assert.ok(settings.includes("invoke<AutostartStatus>('get_autostart_status')"));
  assert.ok(settings.includes("invoke<AutostartStatus>('set_autostart_enabled', { enabled })"));
  assert.ok(settings.includes('autostart?.supported || autostartError'));
  assert.ok(settings.includes('disabled={autostartBusy || !autostart || !!autostartError}'));
  const appSettings = settings.slice(settings.indexOf('interface AppSettings'), settings.indexOf('interface RemoteHealth'));
  assert.ok(!appSettings.includes('autostart'));
});
