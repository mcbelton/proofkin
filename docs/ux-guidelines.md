# UX design guidelines

The web UI (`proofkin --ui`) is where owners run agents, review evidence and
configure what peers may use. These guidelines keep it calm, predictable and
accessible. Every UI change follows them, and the checklist at the end is part of
its definition of done.

## Principles

1. **Clear before clever.** Say what a control does in plain words ("Offer
   server", not "Submit"). Prefer one obvious path over several shortcuts.
2. **Show the consequence first.** Before anything is kept, run or shared, show
   what will happen: a plugin is reviewed before import, a schedule shows its next
   runs, a plan is checked before it runs.
3. **Safe by default.** Defaults pick the least power (read-only access for peers,
   the most restrictive minimum access for MCP servers). Destructive actions ask
   first, in the page, naming what is lost and what stays.
4. **Never lose work.** Typed input survives mode switches, dialog backdrop
   clicks and failed requests. Errors leave the form filled in.
5. **Secrets stay hidden.** Keys and tokens are typed into password fields, are
   never sent back to the page and show as `***` in listings.
6. **Same thing, same look.** A status, an access level or a time looks the same
   everywhere (see the components below).

## Layout

- Three columns on wide screens (sidebar, conversation, panels). From 1180 px down,
  the panels move below and **the page scrolls**, while the conversation keeps the
  window's height beside the sidebar and stays in view (sticky), so its composer is
  always on screen; no region gets a fixed share of the viewport that can squeeze
  another. On phones (760 px and less) one column: conversation, panels, then sidebar.
- The composer never hides its text box or its main button: long helper content
  (the plan explainer) is collapsed by default, remembers whether its owner opened
  it, and the composer scrolls inside itself rather than overflowing.
- Large editing (plans) gets a dedicated full-size editor dialog with the tools for
  the job beside it, instead of a cramped text box.
- Dialogs are at most `100vw - 32px` wide and `100vh - 64px` tall; their body
  scrolls (`overscroll-behavior: contain`), their actions stay visible.

## Design tokens

Use the CSS custom properties in `ui/app.css`; never hard-code colors.

| Token | Use |
| --- | --- |
| `--bg`, `--surface`, `--surface-2`, `--surface-3` | page, cards, insets, chips |
| `--text`, `--muted` | body text, secondary text (contrast at least 4.5:1 on its surface) |
| `--accent`, `--accent-soft` | primary actions, links, selection |
| `--success`, `--warn`, `--danger` (and their `-soft`) | status only, always with a word or icon |
| `--radius`, `--radius-sm` | cards and dialogs, controls |
| `--font`, `--mono` | text; identifiers, commands, plan text |

Spacing follows a 4 px grid (4, 6, 8, 10, 12, 16, 20). Body text is 14 px,
secondary 12.5 px, headings 13 to 16 px; never below 11.5 px.

## Components

- **Buttons:** one primary button per form or dialog (the action it exists for),
  on the right; Cancel to its left. Labels are verbs with an object ("Import
  plugin", "Save changes"). `.danger` for destructive actions. While a request
  runs, the button is disabled and says so ("Importing…").
- **Status chips** (`.status`): Running, Done or Passed, Failed, Paused or Skipped,
  Cancelled. Text always, color second.
- **Badges** (`.badge`): short facts (access level, source, TSG state).
- **Lists:** each item has a title line, a meta line, then actions. Actions sit on
  the item, not hidden in menus.
- **Tabs** (`role="tablist"`): for sibling sections of one dialog; arrow keys move
  between tabs.
- **Confirm dialog:** `confirmDialog()` in `app.js`, never `window.confirm`. The
  title names the object; the body says what is lost and what stays; the confirm
  button repeats the verb ("Delete schedule").
- **Toasts:** confirmation of something that happened ("Imported plugin x"). They
  never carry the only copy of an error that belongs to a form.

## States

Every list and panel defines three states:

- **Empty:** what this is for and the one action that fills it ("No plugins yet.
  Import one from a folder, a GitHub repository or a git URL.").
- **Loading:** keep the previous content, disable the triggering button and change
  its label; for longer waits say what is happening ("Fetching for review…").
- **Error:** inline, next to what failed (`.form-error` with `role="alert"`), in
  the server's words, with the input kept.

**Live work:** work running on a peer says what it is doing now, in a line under
its status ("waiting for a free slot on worker (next in line)", "attempt 2 of 3:
…"), not only "Running". A group of work (a plan, a team run) is one card: a bar
of its parts' states, the parts in the order they can run, and one Cancel for the
whole group. A peer's load is a badge ("idle", "1/2 busy · 1 waiting") with the
details in its tooltip. Updates replace text in place: they never move focus or
the scroll position, and screen readers hear when a part finishes (a polite
status), not every update.

## Forms

- Labels above fields; hints below in `.muted.small`; examples in placeholders end
  with `…` ("e.g. nightly-learn…").
- Validate on submit, show errors inline, and focus the first field in error.
- Use the right control: `type="time"` for times, toggles for days, `select` for
  closed sets, `type="password"` with `autocomplete="off"` for secrets,
  `spellcheck="false"` for identifiers and commands.
- A `select` holds short labels only: its open list is as wide as its longest option,
  can't be styled, and spills over the page. When each choice needs explaining (e.g.
  playbooks), use a picker: radio cards with a one-line description, and the details of
  the picked one beside them.
- Structured input first, free text as the advanced option (the schedule picker
  offers Repeat, time and days; "Custom" accepts the text form; the plan editor's
  Builder offers forms, its Text view the plan syntax, editing the same plan).
- A builder can't express what would be refused: choices come from what exists (peers,
  folders, checks, skills), options that don't apply yet are disabled with the reason,
  and renames and removals carry through. What a field still needs shows once you've
  been there, or on Run, which goes to the first.
- Insert buttons for a text syntax (the plan editor's palette) never paste raw text at
  the cursor: each part goes where the syntax expects it, into the line the cursor is on
  (or a new line), and a placeholder to type over is selected. Errors about that syntax
  show what a correct line looks like.
- A warning says what will happen and what to do, in plain words, on the thing it is
  about (a step, not just a claim id); when the fix is clear, a button makes it.
- Live previews for anything with consequences in time or scope (next runs, what a
  plugin contains, the plan check).

## Microcopy

- Second person, active voice, sentence case. No jargon the owner didn't
  introduce; when a term is needed (MCP, claim, TSG), explain it once nearby.
- Say what will happen and what will not: "Importing runs nothing", "Values stay
  on this machine", "Its past runs stay under Evidence runs".
- Numbers with units; counts that agree ("1 step", "2 steps").

## Time and dates

- Format with `Intl` in the browser's locale: `toLocaleTimeString`,
  `toLocaleDateString`, and `Intl.RelativeTimeFormat` for "in 3 hours" or
  "5 minutes ago". Show the absolute time next to (or as the title of) a relative one.
- A time alone means today; anything older shows its day too ("Yesterday, 9:25 PM",
  "Nov 18, 9:25 PM"), so lists that span days read in order (`stamp()` in `app.js`).
- Schedules are in the agent's local time; say so once in the dialog.

## Accessibility (WCAG 2.2 AA)

- Every control is reachable and usable by keyboard, in a logical order; focus is
  visible (`:focus-visible` outline) and moves into a dialog when it opens and
  back to its opener when it closes. When the opener is gone (it removed its own
  row, or cleared the last item), focus goes to its panel's main action or heading,
  never back to the page.
- Targets are at least 24 × 24 px (WCAG 2.5.8), including checkbox chips.
- Text and icons meet contrast (4.5:1 for text, 3:1 for large text and UI parts) in
  both themes.
- Live regions: the activity feed is `aria-live="polite"`; form errors use
  `role="alert"`; check results use `role="status"`.
- Icons are `aria-hidden`, with visible text or an `aria-label` on their button.
- Respect `prefers-reduced-motion`; no information is conveyed by motion alone.

## Security and trust in the UI

- All peer-provided text is rendered with `textContent` (never `innerHTML`);
  Markdown goes through the escaping renderer.
- Prototypes and review packs open sandboxed in a new tab, detached from this page.
- Links that leave Proofkin (issues, boards and pull requests on GitHub) open in a new
  tab with `rel="noopener"`, and their accessible name says where they go and that a
  new tab opens.
- Show provenance next to anything a peer or a plugin provides (source, commit,
  "from plugin x"), and the access a peer's task needs.

## Theming

- Light and dark themes come from the same tokens; System follows the OS. Set
  `color-scheme` and keep `<meta name="theme-color">` in step with the theme.
- Check every new component in both themes.

## Definition of done (UI changes)

- [ ] Works at 1440×900, 880×760 and 390×844 without clipped or overlapping
      controls; the composer's text box and main button are always visible.
- [ ] Every dropdown opened at 880 px wide stays within its panel (short option labels).
- [ ] Light and dark themes checked.
- [ ] Keyboard only: reach, use and leave every new control and dialog.
- [ ] Empty, loading and error states present and worded per this guide.
- [ ] Destructive actions use the in-page confirm dialog.
- [ ] Secrets never appear in the page or its responses.
- [ ] No console errors; no `innerHTML` with peer or plugin text.
