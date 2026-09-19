import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';

function load(platform) {
  const code = ts.transpileModule(fs.readFileSync('src/utils/codexCommand.ts', 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText;
  const context = { exports: {}, navigator: { platform } };
  vm.runInNewContext(code, context);
  return context.exports;
}

test('Windows produces PowerShell syntax and quotes apostrophes as literal data', () => {
  const api = load('Win32');
  assert.equal(api.isWindows, true);
  assert.equal(api.codexLaunchCommand("http://localhost:18080/v1?x='&y=$()"), "$env:OPENAI_BASE_URL='http://localhost:18080/v1?x=''&y=$()'; codex");
});

test('non-Windows launch command retains the existing POSIX format', () => {
  for (const platform of ['MacIntel', 'Linux x86_64']) {
    const api = load(platform);
    assert.equal(api.isWindows, false);
    assert.equal(api.codexLaunchCommand('http://localhost:18080/v1'), 'OPENAI_BASE_URL=http://localhost:18080/v1 codex');
  }
});

test('displayed and copied local/LAN commands use the same formatter', () => {
  const source = fs.readFileSync('src/components/Proxy.tsx', 'utf8');
  assert.equal(source.match(/codexLaunchCommand\(/g)?.length, 4);
});
