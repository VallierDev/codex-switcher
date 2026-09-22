import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';

const sourceRoot = path.resolve('src');
const localePath = path.join(sourceRoot, 'i18n', 'ru.ts');
const han = /\p{Script=Han}/u;
const normalize = value => value.replace(/\s+/g, ' ').trim();

function sourceFile(file) {
  const kind = file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS;
  return ts.createSourceFile(file, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, kind);
}

function visit(node, callback) {
  callback(node);
  ts.forEachChild(node, child => visit(child, callback));
}

function sourceFiles(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) return sourceFiles(file);
    return [file];
  });
}

const localeSource = sourceFile(localePath);
const translations = new Map();
const errors = [];

visit(localeSource, node => {
  if (!ts.isVariableDeclaration(node) || node.name.getText(localeSource) !== 'replacements') return;
  if (!node.initializer || !ts.isArrayLiteralExpression(node.initializer)) return;
  for (const entry of node.initializer.elements) {
    if (!ts.isArrayLiteralExpression(entry) || entry.elements.length !== 2) {
      errors.push('ru.ts: каталог должен состоять из пар [исходный текст, перевод]');
      continue;
    }
    const [source, target] = entry.elements;
    if (!ts.isStringLiteralLike(source) || !ts.isStringLiteralLike(target)) {
      errors.push('ru.ts: исходный текст и перевод должны быть строковыми литералами');
      continue;
    }
    const key = normalize(source.text);
    if (translations.has(key)) {
      errors.push(`ru.ts: повтор исходной строки ${JSON.stringify(key)}`);
    } else {
      translations.set(key, target.text);
    }
    if (han.test(target.text)) {
      errors.push(`ru.ts: в переводе остались иероглифы: ${JSON.stringify(target.text)}`);
    }
  }
});

if (translations.size === 0) errors.push('ru.ts: каталог переводов не найден или пуст');

for (const [source, target] of translations) {
  const sourcePlaceholders = source.match(/\{[^{}]*\}/g)?.length ?? 0;
  const targetPlaceholders = target.match(/\{[^{}]*\}/g)?.length ?? 0;
  if (sourcePlaceholders !== targetPlaceholders) {
    errors.push(`ru.ts: число подстановок не совпадает: ${JSON.stringify(source)} -> ${JSON.stringify(target)}`);
  }
}

function checkLiteral(file, ast, node, value, kind) {
  const text = normalize(value);
  if (!text || !han.test(text)) return;
  if (translations.has(text)) return;
  const { line } = ast.getLineAndCharacterOfPosition(node.getStart(ast));
  errors.push(`${path.relative(process.cwd(), file)}:${line + 1}: нет точного перевода ${kind} ${JSON.stringify(text)}`);
}

for (const file of sourceFiles(sourceRoot).filter(file => /\.tsx?$/.test(file))) {
  if (file.startsWith(path.join(sourceRoot, 'i18n') + path.sep)) continue;
  const ast = sourceFile(file);
  visit(ast, node => {
    if (ts.isJsxText(node)) {
      checkLiteral(file, ast, node, node.getText(ast), 'JSX-текста');
    } else if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) {
      checkLiteral(file, ast, node, node.text, 'строки');
    } else if (ts.isTemplateHead(node) || ts.isTemplateMiddle(node) || ts.isTemplateTail(node)) {
      checkLiteral(file, ast, node, node.text, 'части шаблона');
    }
  });
}

const rustRoot = path.resolve('src-tauri', 'src');
const rustCandidate = /(Err\b|map_err|ok_or|unwrap_or|return\s+"|=>\s*"|Ok\(|format!|\.into\(\)|to_string\(\))/;
const rustLog = /(println!|eprintln!|tracing::|debug!|info!|warn!|trace!)/;

for (const file of sourceFiles(rustRoot).filter(file => file.endsWith('.rs'))) {
  if (file.includes(`${path.sep}i18n${path.sep}`)) continue;
  let lines = fs.readFileSync(file, 'utf8').split('\n');
  const tests = lines.findIndex(line => /#\[cfg\(test\)\]/.test(line));
  if (tests >= 0) lines = lines.slice(0, tests);
  lines.forEach((line, index) => {
    if (/^\s*\/\//.test(line) || rustLog.test(line) || !rustCandidate.test(line)) return;
    for (const match of line.matchAll(/"((?:\\.|[^"\\])*)"/g)) {
      const text = normalize(match[1].replace(/\\n/g, ' ').replace(/\\"/g, '"'));
      if (!han.test(text) || translations.has(text)) continue;
      errors.push(`${path.relative(process.cwd(), file)}:${index + 1}: нет перевода возможного сообщения backend ${JSON.stringify(text)}`);
    }
  });
}

if (errors.length > 0) {
  console.error(`Проверка локализации не пройдена (${errors.length}):`);
  for (const error of errors) console.error(`- ${error}`);
  process.exitCode = 1;
} else {
  console.log(`Проверка локализации пройдена: ${translations.size} уникальных русских строк.`);
}
