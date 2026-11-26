'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { readConfig } = require('../src/config');

test('readConfig reads one setting per line', () => {
  const text = '# The service\nhost = example.test\r\nport=8080\n\n';
  assert.deepStrictEqual(readConfig(text), { host: 'example.test', port: '8080' });
});

test('readConfig refuses a line that is not key = value', () => {
  assert.throws(() => readConfig('host example.test'), /line 1: expected key = value/);
});
