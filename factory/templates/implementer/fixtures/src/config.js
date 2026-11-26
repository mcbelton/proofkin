'use strict';

// Reading the configuration: one `key = value` setting per line.

/**
 * The settings in `text`, a configuration, by key. Blank lines and lines starting with
 * '#' are skipped; a line that isn't `key = value` is refused.
 */
function readConfig(text) {
  const settings = {};
  text.split(/\r?\n/).forEach((line, index) => {
    const trimmed = line.trim();
    if (trimmed === '' || trimmed.startsWith('#')) {
      return;
    }
    const equals = trimmed.indexOf('=');
    if (equals < 1) {
      throw new Error(`line ${index + 1}: expected key = value, got "${trimmed}"`);
    }
    settings[trimmed.slice(0, equals).trim()] = trimmed.slice(equals + 1).trim();
  });
  return settings;
}

module.exports = { readConfig };
