import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';

const source = fs.readFileSync('src/i18n/ru.ts', 'utf8');
const code = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;
const module = { exports: {} };
vm.runInNewContext(code, { module, exports: module.exports });
const { translateRu } = module.exports;

function transpileModule(file, context = {}) {
  const source = fs.readFileSync(file, 'utf8');
  const code = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const loaded = { exports: {} };
  vm.runInNewContext(code, {
    module: loaded,
    exports: loaded.exports,
    require: name => context[name] ?? {},
  });
  return loaded.exports;
}

const { resolveAppLocale } = transpileModule('src/i18n/index.ts', {
  './ru': { russianLocale: {} },
  './runtime': { installUiLocale() {} },
});

test('system locale selects Russian and unsupported locales fall back to Chinese', () => {
  assert.equal(resolveAppLocale('ru'), 'ru');
  assert.equal(resolveAppLocale('ru-RU'), 'ru');
  assert.equal(resolveAppLocale('zh-CN'), 'zh-CN');
  assert.equal(resolveAppLocale('en-US'), 'zh-CN');
  assert.equal(resolveAppLocale(''), 'zh-CN');
  assert.equal(resolveAppLocale('en-US', 'ru'), 'ru');
  assert.equal(resolveAppLocale('ru-RU', 'zh-CN'), 'zh-CN');
  assert.equal(resolveAppLocale('ru-RU', 'unsupported'), 'ru');
  assert.equal(resolveAppLocale('ru-RU', 'auto'), 'ru');
});

test('dynamic backend messages retain values and prefer the most specific template', () => {
  const cases = [
    ['模型列表请求失败: timeout', 'Не удалось запросить список моделей: timeout'],
    [
      'Server 不可达（primary=http://a, fallback=http://b）',
      'Server недоступен (основной адрес: http://a, резервный: http://b)',
    ],
    ['已重置 2 个限额窗口', 'Сброшено окон лимита: 2'],
  ];
  for (const [input, expected] of cases) assert.equal(translateRu(input), expected);
});

test('complete messages win over fragment replacements', () => {
  assert.equal(
    translateRu('Fast 模式已开启（2x 额度消耗，更快推理）。重启 Codex 生效。'),
    'Режим Fast включён: ответы быстрее, расход квоты удвоен. Перезапустите Codex для применения',
  );
});
