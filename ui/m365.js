// Proofkin web UI: Microsoft 365 content in plain words. How the page says the labels of
// what Work IQ brought (never the internal `unknown`), how many results were kept and
// withheld, who answers a @workiq step, where a knowledge topic came from, and a route's
// error on its own. Pure helpers, tested in node (tests/m365.test.cjs) as the page loads
// them.
"use strict";

/** The labels Proofkin gives Microsoft 365 content Work IQ gave no label information for,
 * and content without a label (src/workiq/content.rs UNKNOWN and UNLABELED). */
const M365_UNKNOWN = "unknown";
const M365_UNLABELED = "unlabeled";

/** A sensitivity label as the page says it: `unknown` is "no label information". */
function m365Label(label) {
  return label === M365_UNKNOWN ? "no label information" : label;
}

/** Labels as the page says them, comma separated. */
function m365Labels(labels) {
  return labels.map(m365Label).join(", ");
}

/** How many Microsoft 365 results were kept, with their labels, and how many were withheld
 * ("3 results kept (General), 2 withheld"; "1 result kept (no label information), none
 * withheld"). */
function workiqCounts(kept, labels, withheld) {
  const named = labels?.length ? ` (${m365Labels(labels)})` : "";
  return `${kept} result${kept === 1 ? "" : "s"} kept${named}, ${withheld ? `${withheld} withheld` : "none withheld"}`;
}

/** Who answers a @workiq step, from its task's target (`workiq`, or `workiq:<agent id>` when
 * it asks one of the account's declarative agents): Microsoft 365 Copilot through Work IQ,
 * not a provider or a peer. */
function workiqWho(target) {
  const agent = /^@?workiq:(.+)$/.exec(target ?? "")?.[1];
  return agent ? `Microsoft 365 agent ${agent} (Work IQ)` : "Microsoft 365 Copilot (Work IQ)";
}

/** Where a knowledge topic came from, for its line in the Knowledge list: for one imported
 * from Microsoft 365 (`labels`: its mark's labels, see GET /api/ops), its labels and a title
 * saying who it may go to; null for any other topic (no mark). A mark without labels is
 * content without a label, as src/knowledge.rs labels_of_mark reads it. */
function knowledgeOrigin(labels) {
  if (!labels) return null;
  const shown = labels.length ? labels : [M365_UNLABELED];
  return {
    text: `Microsoft 365 · ${m365Labels(shown)}`,
    title: shown.includes(M365_UNKNOWN)
      ? "Imported from Microsoft 365. Content without label information never goes to a peer, so neither does this topic"
      : "Imported from Microsoft 365: it goes only to peers whose Microsoft 365 grant covers these labels",
  };
}

/** `text` as a sentence of its own, starting with a capital: routes word some errors to
 * follow a colon, as the command line prints them. */
function asSentence(text) {
  return text ? text.charAt(0).toUpperCase() + text.slice(1) : text;
}
