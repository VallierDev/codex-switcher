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
  './en': { englishLocale: {} },
  './runtime': { installUiLocale() {} },
});

test('system locale selects Russian and unsupported locales fall back to Chinese', () => {
  assert.equal(resolveAppLocale('ru'), 'ru');
  assert.equal(resolveAppLocale('ru-RU'), 'ru');
  assert.equal(resolveAppLocale('zh-CN'), 'zh-CN');
  assert.equal(resolveAppLocale('en-US'), 'en');
  assert.equal(resolveAppLocale('en-GB'), 'en');
  assert.equal(resolveAppLocale('de-DE'), 'zh-CN');
  assert.equal(resolveAppLocale(''), 'zh-CN');
  assert.equal(resolveAppLocale('en-US', 'ru'), 'ru');
  assert.equal(resolveAppLocale('ru-RU', 'zh-CN'), 'zh-CN');
  assert.equal(resolveAppLocale('ru-RU', 'en'), 'en');
  assert.equal(resolveAppLocale('ru-RU', 'unsupported'), 'ru');
  assert.equal(resolveAppLocale('ru-RU', 'auto'), 'ru');
});

const translator = transpileModule('src/i18n/translator.ts');
const { translateEn, englishReplacements } = transpileModule('src/i18n/en.ts', { './translator': translator });
test('English catalog covers every Russian source key and preserves placeholders', () => {
  const english = new Map(englishReplacements);
  const russianAst = ts.createSourceFile('ru.ts', source, ts.ScriptTarget.Latest, true);
  function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(russianAst) === 'replacements') {
      for (const pair of node.initializer.elements) {
        const key = pair.elements[0].text;
        assert.ok(english.has(key), `Missing English translation: ${key}`);
        const value = english.get(key);
        assert.ok(!/[\p{Script=Han}\p{Script=Cyrillic}]/u.test(value), `Untranslated English value: ${key}`);
        assert.equal(value.match(/\{[^{}]*\}/g)?.length ?? 0, key.match(/\{[^{}]*\}/g)?.length ?? 0, key);
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(russianAst);
});

test('English dynamic messages preserve names and URLs', () => {
  assert.equal(translateEn('账号'), 'Account');
  assert.equal(translateEn('模型列表请求失败: timeout'), 'Model list request failed: timeout');
  assert.equal(translateEn('Server 不可达（primary=http://a, fallback=http://b）'), 'Server unreachable (primary=http://a, fallback=http://b)');
  assert.equal(translateEn('请求失败: 生产账号A'), 'Request failed: 生产账号A');
  assert.equal(translateEn('1小时 2分钟 3秒 后重置'), '1h 2m 3s until reset');
  assert.equal(translateEn('3天 2小时 10分钟 后重置'), '3d 2h 10m until reset');
});

test('updated invitation messages coexist with localization fixes and preserve entrypoint codes', () => {
  const cases = [
    [
      '本次查询未显示邀请入口，不代表邀请次数为零。',
      'В этом ответе вход для приглашений не показан; это не означает нулевой остаток.',
      'This query did not show an invitation entrypoint; this does not mean zero invitations remain.',
    ],
    [
      '浏览器已确认 persistent 入口；发送邀请请在官方 Desktop 完成。',
      'Браузер подтвердил persistent вход; отправляйте приглашения в официальном Desktop.',
      'Browser verified persistent entrypoint; send invitations in official Desktop.',
    ],
    [
      '浏览器已查询 persistent / rate_limit，接口未展示入口；不代表邀请次数为零。',
      'Браузер проверил persistent / rate_limit; API не показал вход. Это не означает нулевой остаток приглашений.',
      'Browser checked persistent / rate_limit; the API did not show an entrypoint. This does not mean zero invitations remain.',
    ],
  ];
  for (const [input, russian, english] of cases) {
    assert.equal(translateRu(input).replace(/\s+/g, ' '), russian);
    assert.equal(translateEn(input).replace(/\s+/g, ' '), english);
  }
  for (const code of ['persistent', 'rate_limit']) {
    assert.equal(translateRu(code), code);
    assert.equal(translateEn(code), code);
  }
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

test('Russian UI captions cover filters, tray, import and compact durations', () => {
  const cases = [
    ['ALL', 'Все'], ['Sub', 'Подписки'], ['Usage', 'Квоты'],
    ['332 Team', '332 командный'], ['→ Switch', '→ Переключить'],
    ['/ Out', '/ Выход'], ['header。', 'заголовок.'],
    ['5H', '5ч'], ['5h', '5ч'], ['5h 35%', '5ч 35%'],
    ['5h min–max', 'Мин. / макс. 5ч'],
    ['2h 35m', '2 ч 35 мин'], ['2h35m', '2 ч 35 мин'],
    ['9m', '9 мин'], ['45s', '45 с'],
    ['🌿 周期保鲜 · 5H', '🌿 Автозапуск периода · 5ч'],
    ['🌿 周期保鲜 · 7D', '🌿 Автозапуск периода · 7д'],
    ['🌿 保鲜已关 · 5H', '🌿 Автозапуск отключён · 5ч'],
    ['🌿 保鲜已关 · 7D', '🌿 Автозапуск отключён · 7д'],
  ];
  for (const [input, expected] of cases) assert.equal(translateRu(input), expected);
  for (const value of [
    'current', 'future', 'expired_or_near_expiry', 'invalid_timestamp', 'unparsed',
    'proxy_threshold_5h', 'gpt-5h', 'quota_5h', 'https://example.com/5h',
  ]) assert.equal(translateRu(value), value);
});

test('Turn-State time codes have display-only captions with an unknown-code fallback', () => {
  const statsSource = fs.readFileSync('src/components/Stats.tsx', 'utf8');
  const ast = ts.createSourceFile('Stats.tsx', statsSource, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const label = ast.statements.find(node => (
    ts.isFunctionDeclaration(node) && node.name?.text === 'turnStateTimeLabel'
  ));
  assert.ok(label);
  const loaded = { exports: {} };
  const code = ts.transpileModule(label.getText(ast) + '\nexport { turnStateTimeLabel };', {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  vm.runInNewContext(code, { module: loaded, exports: loaded.exports });
  const { turnStateTimeLabel } = loaded.exports;
  const cases = [
    ['unparsed', '时间未解析', 'Время не разобрано', 'Time not parsed'],
    ['invalid_timestamp', '时间戳无效', 'Некорректная временная метка', 'Invalid timestamp'],
    ['future', '时间在未来', 'Время в будущем', 'Time is in the future'],
    ['expired_or_near_expiry', '已过期或即将过期', 'Истёк или скоро истечёт', 'Expired or near expiry'],
    ['current', '时间有效', 'Актуален', 'Current'],
  ];
  for (const [status, chinese, russian, english] of cases) {
    assert.equal(turnStateTimeLabel(status), chinese);
    assert.equal(translateRu(chinese), russian);
    assert.equal(translateEn(chinese), english);
  }
  assert.equal(turnStateTimeLabel('new_server_status'), 'new_server_status');
  assert.ok(statsSource.includes('turnStateTimeLabel(item.time_status)'));
});

test('recovery errors are localized without translating identities or nested diagnostics', () => {
  const panel = fs.readFileSync('src/components/AnchorRecoveryPanel.tsx', 'utf8');
  assert.ok(panel.includes('<p role="alert">{error}</p>'));
  assert.ok(panel.includes('value={account.id} translate="no">{account.name}'));
  assert.ok(panel.includes('<code>{reason}</code>'));
  assert.equal(
    translateRu('Phone anchor changed. Refresh the recovery panel and try again.'),
    'Аккаунт мобильной привязки изменился. Обновите панель восстановления и повторите попытку.',
  );
  assert.equal(
    translateRu('Anchor save failed: EIO; disk rollback failed: 生产账号A. Restore the previous anchor before continuing.'),
    'Не удалось сохранить мобильную привязку: EIO; не удалось восстановить файл на диске: 生产账号A. Восстановите прежнюю привязку, прежде чем продолжить.',
  );
  assert.equal(translateRu('Google OAuth token request failed with HTTP 401 Unauthorized'),
    'Ошибка запроса токена OAuth Google: HTTP 401 Unauthorized');
});

test('runtime leaves opted-out user text and attributes unchanged', () => {
  const source = fs.readFileSync('src/i18n/runtime.ts', 'utf8')
    + '\nexport { translateTextNode, translateElementAttributes };';
  const loaded = { exports: {} };
  vm.runInNewContext(ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText, { module: loaded, exports: loaded.exports });
  const { translateTextNode, translateElementAttributes } = loaded.exports;
  const locale = { translate: translateRu };
  for (const marker of ['[translate="no"]', '[data-i18n-ignore]']) {
    const attributes = new Map([['title', '生产账号A']]);
    const parent = {
      closest: selector => selector.includes(marker) ? parent : null,
      getAttribute: key => attributes.get(key),
      setAttribute: (key, value) => attributes.set(key, value),
    };
    const text = { data: '生产账号A', parentElement: parent };
    translateTextNode(text, locale);
    translateElementAttributes(parent, locale);
    assert.equal(text.data, '生产账号A');
    assert.equal(attributes.get('title'), '生产账号A');
  }
  const text = { data: '账号', parentElement: { closest: () => null } };
  translateTextNode(text, locale);
  assert.equal(text.data, 'Аккаунт');
});
