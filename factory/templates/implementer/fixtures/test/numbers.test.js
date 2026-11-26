'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { round, sum } = require('../src/numbers');

test('sum adds the values up', () => {
  assert.strictEqual(sum([1, 2, 3.5]), 6.5);
  assert.strictEqual(sum([]), 0);
});

test('round keeps the decimals asked for', () => {
  assert.strictEqual(round(3.14159, 2), 3.14);
  assert.strictEqual(round(2.5), 3);
});
