'use strict';

// Helpers for text.

/** `text` with its first letter in upper case. */
function capitalize(text) {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** The words of `text`: what is between its spaces. */
function words(text) {
  return text.split(/\s+/).filter((word) => word !== '');
}

module.exports = { capitalize, words };
