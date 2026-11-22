// Minimal, safe Markdown (task results, task and step text): builds DOM nodes with
// textContent only, so markup in the text stays text. Loaded before app.js; it needs
// nothing from it.
"use strict";

function mdEl(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else node.setAttribute(key, value);
  }
  for (const child of children.flat()) node.append(child);
  return node;
}

function inlineMd(text) {
  const nodes = [];
  const re = /(`[^`]+`|\*\*[^*]+\*\*)/g;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) nodes.push(text.slice(last, m.index));
    const t = m[0];
    nodes.push(t.startsWith("`") ? mdEl("code", { text: t.slice(1, -1) }) : mdEl("strong", { text: t.slice(2, -2) }));
    last = m.index + t.length;
  }
  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
}

function mdTable(rows) {
  const cells = (row) => row.trim().replace(/^\||\|$/g, "").split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, "|"));
  const [head, ...body] = rows.filter((r) => !/^\s*\|?\s*:?-{3,}/.test(r));
  // Not a real table (e.g. only a separator line): show the text as is.
  if (head === undefined) return mdEl("p", { text: rows.join("\n") });
  return mdEl("div", { class: "table-wrap" }, mdEl("table", {},
    mdEl("thead", {}, mdEl("tr", {}, ...cells(head).map((c) => mdEl("th", { scope: "col" }, inlineMd(c))))),
    mdEl("tbody", {}, ...body.map((r) => mdEl("tr", {}, ...cells(r).map((c) => mdEl("td", {}, inlineMd(c))))))));
}

function renderMarkdown(text) {
  const root = mdEl("div", { class: "md" });
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  let list = null;
  let listType = null;
  let para = [];
  const flushPara = () => {
    if (para.length) root.append(mdEl("p", {}, inlineMd(para.join(" "))));
    para = [];
  };
  const endList = () => { list = null; listType = null; };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    // Collapsible sections in evidence reports: rendered as <details>, content as Markdown.
    const details = line.match(/^<details><summary>(.*)<\/summary>$/);
    if (details) {
      flushPara(); endList();
      const inner = [];
      for (i++; i < lines.length && lines[i].trim() !== "</details>"; i++) inner.push(lines[i]);
      root.append(mdEl("details", {}, mdEl("summary", { text: details[1] }), renderMarkdown(inner.join("\n"))));
      continue;
    }
    if (/^\s*\|/.test(line)) {
      flushPara(); endList();
      const rows = [];
      for (; i < lines.length && /^\s*\|/.test(lines[i]); i++) rows.push(lines[i]);
      i--;
      root.append(mdTable(rows));
      continue;
    }
    if (/^\s*```/.test(line)) {
      flushPara(); endList();
      const code = [];
      for (i++; i < lines.length && !/^\s*```/.test(lines[i]); i++) code.push(lines[i]);
      root.append(mdEl("pre", {}, mdEl("code", { text: code.join("\n") })));
      continue;
    }
    const heading = line.match(/^(#{1,4})\s+(.*)$/);
    if (heading) {
      flushPara(); endList();
      root.append(mdEl(`h${Math.min(6, heading[1].length + 2)}`, {}, inlineMd(heading[2])));
      continue;
    }
    const item = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      flushPara();
      const type = /\d/.test(item[1]) ? "ol" : "ul";
      if (!list || listType !== type) {
        list = mdEl(type);
        listType = type;
        root.append(list);
      }
      list.append(mdEl("li", {}, inlineMd(item[2])));
      continue;
    }
    if (!line.trim()) { flushPara(); endList(); continue; }
    if (list && /^\s{2,}/.test(line)) { list.lastChild.append(" ", ...inlineMd(line.trim())); continue; }
    endList();
    para.push(line.trim());
  }
  flushPara();
  return root;
}
