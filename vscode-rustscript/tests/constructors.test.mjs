import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const grammar = JSON.parse(readFileSync(new URL('../syntaxes/rustscript.tmLanguage.json', import.meta.url), 'utf8'));
const constructors = grammar.repository['option-result-constructor'].patterns;
const option = new RegExp(constructors[0].match, 'g');
const result = new RegExp(constructors[1].match, 'g');

test('bare Option constructors are highlighted in expressions and match patterns', () => {
  const source = 'Some(1); None; match value { Some(x) => x, None => 0 }';
  assert.deepEqual([...source.matchAll(option)].map(([name]) => name), ['Some', 'None', 'Some', 'None']);
});

test('removed namespaced Option spelling and member access are not constructors', () => {
  for (const source of ['Option::Some(1)', 'Option::None', 'obj.None', 'obj.Some(1)']) {
    assert.equal([...source.matchAll(option)].length, 0, source);
  }
  const patterns = grammar.patterns.map(({ include }) => include);
  assert.ok(patterns.indexOf('#strings') < patterns.indexOf('#option-result-constructor'));
  assert.deepEqual([...`Result::Ok(1); Result::Err("no")`.matchAll(result)].map(([name]) => name), ['Result::Ok', 'Result::Err']);
});
