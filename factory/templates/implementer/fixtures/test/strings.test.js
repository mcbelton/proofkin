'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { capitalize, words } = require('../src/strings');

test('capitalize puts the first letter in upper case', () => {
  assert.strictEqual(capitalize('hello world'), 'Hello world');
  assert.strictEqual(capitalize(''), '');
});

test('words are what is between the spaces', () => {
  assert.deepStrictEqual(words('  Hello   big world '), ['Hello', 'big', 'world']);
  assert.deepStrictEqual(words(''), []);
});
