// Cases for the Markdown renderer (ui/markdown.js) that shows task and step text.
// tests/ui.rs runs this with node, with a minimal fake document.
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const text = (value) => ({ nodeType: 3, textContent: value, children: [] });
const document = {
  createElement(tag) {
    const node = { nodeType: 1, tag, attrs: {}, children: [], className: "" };
    node.setAttribute = (k, v) => { node.attrs[k] = v; };
    node.append = (...items) => {
      for (const i of items) node.children.push(typeof i === "string" ? text(i) : i);
    };
    Object.defineProperty(node, "lastChild", { get: () => node.children[node.children.length - 1] });
    Object.defineProperty(node, "textContent", {
      get: () => node.children.map((c) => c.textContent).join(""),
      set: (v) => { node.children = [text(v)]; },
    });
    return node;
  },
};

const code = fs.readFileSync(path.join(__dirname, "..", "ui", "markdown.js"), "utf8");
const { renderMarkdown } = vm.runInNewContext(`${code}\n;({ renderMarkdown })`, { document });

const tags = (node) => node.children.map((c) => c.tag);
const all = (node) => [node, ...node.children.flatMap(all)].filter((n) => n.nodeType === 1);

const root = renderMarkdown(
  "Add tests for `parse()` and **fix** the bug.\n\n- first item\n- second item\n\nThen run them.",
);
assert.strictEqual(root.className, "md");
assert.deepStrictEqual(tags(root), ["p", "ul", "p"]);
const [intro, list, outro] = root.children;
assert.deepStrictEqual(tags(intro), [undefined, "code", undefined, "strong", undefined]);
assert.strictEqual(intro.children[1].textContent, "parse()");
assert.strictEqual(intro.children[3].textContent, "fix");
assert.deepStrictEqual(tags(list), ["li", "li"]);
assert.strictEqual(list.children[1].textContent, "second item");
assert.strictEqual(outro.textContent, "Then run them.");

// Markup in the text stays text: the only elements are the ones the renderer builds.
const hostile = renderMarkdown('<img src=x onerror="alert(1)"> and `<b>x</b>` <script>bad()</script>');
assert.deepStrictEqual(all(hostile).map((n) => n.tag), ["div", "p", "code"]);
assert.ok(hostile.textContent.includes('<img src=x onerror="alert(1)">'));
assert.ok(hostile.textContent.includes("<script>bad()</script>"));
assert.strictEqual(all(hostile)[2].textContent, "<b>x</b>");

// A signed result's summary sits under a rule; a rule right under a text line stays text.
const signed = renderMarkdown("pong\n\n---\n**Model** `claude-opus-5.5` (copilot)\n\n* * *\n- a\n___");
assert.deepStrictEqual(tags(signed), ["p", "hr", "p", "hr", "ul", "hr"]);
assert.strictEqual(signed.children[2].textContent, "Model claude-opus-5.5 (copilot)");
const under = renderMarkdown("Title\n---\nbody");
assert.deepStrictEqual(tags(under), ["p"]);
assert.strictEqual(under.textContent, "Title --- body");

console.log("ok: task text becomes code, list items, rules and paragraphs, and markup stays text");
