'use strict';

// Helpers for numbers.

/** The sum of `values`. */
function sum(values) {
  return values.reduce((total, value) => total + value, 0);
}

/** `value` rounded to `digits` decimals. */
function round(value, digits = 0) {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

module.exports = { sum, round };
