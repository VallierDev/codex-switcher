import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import ts from 'typescript';

const file = ts.createSourceFile('AddAccountModal.tsx', fs.readFileSync('src/components/AddAccountModal.tsx', 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const expressions = [];
function visit(node) {
  if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.name.text === 'filename') expressions.push(node.initializer.getText(file));
  ts.forEachChild(node, visit);
}
visit(file);
assert.equal(expressions.length, 1);
const basename = vm.runInNewContext(`p => (${expressions[0]})`);

test('import filename excludes Windows drive and UNC directories', () => {
  assert.equal(basename('C:\\Users\\한글 이름\\accounts.json'), 'accounts.json');
  assert.equal(basename('\\\\server\\share\\tokens.zip'), 'tokens.zip');
});
test('POSIX, mixed separators and plain filenames stay valid', () => {
  for (const p of ['/tmp/accounts.json', 'C:\\Users/folder\\accounts.json', 'accounts.json']) assert.equal(basename(p), 'accounts.json');
});
