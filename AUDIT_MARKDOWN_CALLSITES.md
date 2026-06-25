# Audit of render_markdown Callsites in ui.rs

## Summary
This document audits all production callsites of `crate::markup::render_markdown()` in `crates/makina/src/ui.rs`.

**Current Signature:**
```rust
pub fn render_markdown(text: &str, base: Style, width: u16) -> Vec<Line<'static>>
```

**Audit Date:** 2026-06-25  
**Total Callsites Found:** 6 (5 production, 1 test reference)

---

## Production Callsites

### 1. Line 1047: render_plan_task_pane

**Location:** `crates/makina/src/ui.rs:1047`

**Function Signature:**
```rust
fn render_plan_task_pane(
    app: &App,
    plan: &makina_core::orchestrator::PlanEntry,
    preview: &makina_core::orchestrator::PlanTaskPreview,
    frame: &mut Frame,
    area: Rect,
)
```

**Callsite Code:**
```rust
for l in crate::markup::render_markdown(&preview.body, base_style, content_area.width) {
    push_line!(l);
}
```

**Context (lines 1045-1049):**
```rust
let base_style =
    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
for l in crate::markup::render_markdown(&preview.body, base_style, content_area.width) {
    push_line!(l);
}
```

**Availability of `app`:** ✅ YES - `app` is a direct function parameter  
**Scope:** Production (not in test block)  
**Can Thread Theme:** ✅ YES - `app` is directly available

---

### 2. Line 1829: render_task_entry_pane

**Location:** `crates/makina/src/ui.rs:1829`

**Function Signature:**
```rust
fn render_task_entry_pane(
    app: &App,
    run: &RunView,
    task_idx: usize,
    frame: &mut Frame,
    area: Rect,
)
```

**Callsite Code:**
```rust
lines.extend(crate::markup::render_markdown(
    &task.entry_text,
    base_style,
    content_width,
));
```

**Context (lines 1826-1833):**
```rust
let content_width = inner.width;
let base_style =
    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
lines.extend(crate::markup::render_markdown(
    &task.entry_text,
    base_style,
    content_width,
));
```

**Availability of `app`:** ✅ YES - `app` is a direct function parameter  
**Scope:** Production (not in test block)  
**Can Thread Theme:** ✅ YES - `app` is directly available

---

### 3. Line 2144: render_accordion_section

**Location:** `crates/makina/src/ui.rs:2144`

**Function Signature:**
```rust
fn render_accordion_section(
    app: &App,
    title: &str,
    section: AccordionSection,
    expanded_set: &HashSet<AccordionSection>,
    content: &str,
    focused: bool,
    content_width: u16,
    as_markdown: bool,
) -> Vec<Line<'static>>
```

**Callsite Code:**
```rust
for mut line in crate::markup::render_markdown(content, base, body_width) {
    line.spans.insert(0, Span::raw("  "));
    result.push(line);
}
```

**Context (lines 2141-2147):**
```rust
let body_width = content_width.saturating_sub(2);
let base =
    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
for mut line in crate::markup::render_markdown(content, base, body_width) {
    line.spans.insert(0, Span::raw("  "));
    result.push(line);
}
```

**Availability of `app`:** ✅ YES - `app` is a direct function parameter  
**Scope:** Production (not in test block)  
**Can Thread Theme:** ✅ YES - `app` is directly available

---

### 4. Line 2472: exchange_entry_lines

**Location:** `crates/makina/src/ui.rs:2472`

**Function Signature:**
```rust
fn exchange_entry_lines(entry: &ExchangeEntry, app: &App, width: u16) -> Vec<Line<'static>>
```

**Callsite Code:**
```rust
lines.extend(crate::markup::render_markdown(text, base_style, width));
```

**Context (lines 2470-2472):**
```rust
// Response text rendered through Markdown + ANSI.
let base_style = Style::default().fg(resp_color);
lines.extend(crate::markup::render_markdown(text, base_style, width));
```

**Availability of `app`:** ✅ YES - `app` is a direct function parameter  
**Scope:** Production (not in test block)  
**Can Thread Theme:** ✅ YES - `app` is directly available

---

### 5. Line 2520: exchange_entry_lines (same function)

**Location:** `crates/makina/src/ui.rs:2520`

**Function Signature:** Same as callsite #4  
```rust
fn exchange_entry_lines(entry: &ExchangeEntry, app: &App, width: u16) -> Vec<Line<'static>>
```

**Callsite Code:**
```rust
let mut thought_lines = crate::markup::render_markdown(text, base_style, width);
```

**Context (lines 2517-2525):**
```rust
// Thought body only in verbose mode.
if app.verbose_mode {
    let base_style =
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim));
    let mut thought_lines = crate::markup::render_markdown(text, base_style, width);
    // Indent all thought lines by 2 spaces.
    for line in &mut thought_lines {
        line.spans.insert(0, Span::raw("  "));
    }
    lines.extend(thought_lines);
}
```

**Availability of `app`:** ✅ YES - `app` is a direct function parameter  
**Scope:** Production (not in test block)  
**Can Thread Theme:** ✅ YES - `app` is directly available

---

## Test-Related References

### Line 1765: Comment (not a callsite)
**Location:** `crates/makina/src/ui.rs:1765`

```rust
/// the body reuses plan 0020's hardened `render_markdown` — no new parser.
```

This is a documentation comment, not a function call.

---

### Line 5833: exchange_render_markdown_and_ansi (test function name)
**Location:** `crates/makina/src/ui.rs:5833`

```rust
#[test]
fn exchange_render_markdown_and_ansi() {
```

This is the name of a test function that validates markdown rendering in exchanges. The test does NOT directly call `render_markdown`; it calls the render functions through the app's render pipeline.

---

### Line 8814: Comment in test section
**Location:** `crates/makina/src/ui.rs:8814`

```rust
/// and done_when) processed through render_markdown.
```

This is a documentation comment in a test module, not a function call.

---

### Line 8872: Comment in test section
**Location:** `crates/makina/src/ui.rs:8872`

```rust
/// render_markdown using the pane's inner width for proper text wrapping.
```

This is a documentation comment in a test module (`task_entry_pane_respects_pane_width` test), not a function call.

---

## Summary Table

| # | Line | Function | App In Scope | Scope Type | Can Thread Theme |
|---|------|----------|--------------|------------|------------------|
| 1 | 1047 | `render_plan_task_pane` | ✅ Yes (parameter) | Production | ✅ Yes |
| 2 | 1829 | `render_task_entry_pane` | ✅ Yes (parameter) | Production | ✅ Yes |
| 3 | 2144 | `render_accordion_section` | ✅ Yes (parameter) | Production | ✅ Yes |
| 4 | 2472 | `exchange_entry_lines` | ✅ Yes (parameter) | Production | ✅ Yes |
| 5 | 2520 | `exchange_entry_lines` | ✅ Yes (parameter) | Production | ✅ Yes |

---

## Key Findings

1. **Total Production Callsites:** 5 (all in `crates/makina/src/ui.rs`)

2. **App Availability:** ✅ ALL 5 production callsites have `app` directly available as a function parameter.

3. **Threading Theme Parameter:** ✅ ALL 5 production callsites can be updated to pass `&app.active_theme` without requiring parameter threading. No helper functions need modifications.

4. **Functions Containing Callsites:**
   - `render_plan_task_pane` (line 959-1060) — Renders plan task preview body
   - `render_task_entry_pane` (line 1766-1846) — Renders running task entry text
   - `render_accordion_section` (line 2096-2158) — Renders plan accordion sections (SCOPE, ARCHITECTURE, TASKS, STATUS)
   - `exchange_entry_lines` (line 2415-2600+) — Renders agent exchange entries (prompts, responses, thoughts)

5. **Test Coverage:** The tests at line 5833 (`exchange_render_markdown_and_ansi`), line 8816 (`task_entry_pane_renders_markdown`), and line 8874 (`task_entry_pane_respects_pane_width`) exercise the markdown rendering indirectly through the full render pipeline, confirming that these callsites are well-tested.

---

## Next Steps

To thread the theme parameter through all callsites, the following tasks will:

1. **Task `add-codblock-theme-role`:** Add `CodeBlock` role to `ThemeRole` enum and all three Ayu variants.

2. **Task `update-render-markdown-signature`:** Update the function signature to:
   ```rust
   pub fn render_markdown(text: &str, base: Style, width: u16, theme: &crate::theme::Theme) -> Vec<Line<'static>>
   ```

3. **Task `replace-code-block-hardcoded-modifiers`:** Replace hardcoded `Modifier::DIM | Modifier::REVERSED` with theme-aware colors using `theme.get(ThemeRole::CodeBlock)`.

4. **Task `thread-theme-through-render-markdown-callsites`:** Update all 5 production callsites to pass `&app.active_theme` as the fourth argument. No function signatures need changes since all have `app` in scope.

---

## Verification

✅ All acceptance criteria met:
- List of all `render_markdown` callsites in ui.rs documented (file:line + function name + availability of app)
- At least 3 production callsites identified (5 total)
- All sites note whether app is in scope or must be threaded (all are in scope, no threading needed)
- This is an exploration task with no tests to break
- cargo test/clippy/fmt will remain green
