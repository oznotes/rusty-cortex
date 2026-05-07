# Rusty Cortex — Design DNA

**This is the source of truth for all visual decisions.** Every component, every new feature, every future agent must follow these rules. Nothing is random. Everything has a system.

---

## Philosophy

Rusty Cortex looks like **GitHub Desktop**, not like a consumer app. It is a professional tool for technicians who use it every day. The design is:

- **Systematic** — every value comes from a defined scale, never ad-hoc
- **Restrained** — one accent color, muted surfaces, no gradients, no glow, no emoji in UI
- **Cohesive** — if you can't point to the rule that produced a value, that value is wrong
- **Dense** — show information, don't hide it behind tabs or modals

## Layout

```
┌──────────────────────────────────────────────────────┐
│  Title Bar (40px height)                             │
├──────────────┬───────────────────────────────────────┤
│  Sidebar     │  Workspace                            │
│  (200px)     │  (flexible)                           │
│              │                                       │
├──────────────┴───────────────────────────────────────┤
│  Log Panel (collapsible)                             │
└──────────────────────────────────────────────────────┘
```

- **Sidebar:** 200px fixed, darker surface, always visible
- **Workspace:** Fills remaining width
- **Log Panel:** Docked at bottom, full width, collapsible
- **Title Bar:** 40px, app name left, version/theme toggle right

## Typography

Two font families. No exceptions.

| Role | Family | Fallback Stack |
|------|--------|----------------|
| UI chrome | Inter | -apple-system, 'Segoe UI', system-ui, sans-serif |
| Code values | Cascadia Code | 'Fira Code', Consolas, monospace |

**When to use monospace:** serial numbers, file names, file paths, log entries, partition names — anything the user might copy-paste or that comes from a machine.

**When to use Inter:** labels, headings, buttons, descriptions — anything the human wrote.

### Type Scale

Only these sizes exist. No in-between values.

| Token | Size | Use |
|-------|------|-----|
| `--font-xs` | 9px | Timestamps in log, tertiary info |
| `--font-sm` | 11px | Section headers (uppercase), small labels, log entries |
| `--font-base` | 12px | Labels, body text, inputs, buttons |
| `--font-md` | 13px | Device name, app title, primary content |
| `--font-lg` | 16px | Reserved for future emphasis (not currently used) |

### Font Weight Scale

| Token | Weight | Use |
|-------|--------|-----|
| Regular | 400 | Body text, code values, descriptions |
| Medium | 500 | Labels, secondary buttons |
| Semibold | 600 | Section headers, device name, app title, primary buttons |

No bold (700). No light (300). If it needs more emphasis, it's semibold. If it doesn't, it's regular.

### Section Headers

All section headers follow this exact pattern:
```css
font-size: 11px;
font-weight: 600;
text-transform: uppercase;
letter-spacing: 0.05em;
color: var(--text-label);
margin-bottom: 12px;
```

No variation. Every section header in the app looks identical.

## Spacing

Base unit: **4px**. All spacing is a multiple of 4.

| Use | Value | Multiple |
|-----|-------|----------|
| Tight gap (between related items) | 4px | 1x |
| Small gap (between buttons, list items) | 8px | 2x |
| Medium gap (between form fields) | 12px | 3x |
| Standard padding (inside components) | 16px | 4x |
| Section gap (between sidebar sections) | 20px | 5x |
| Large gap (workspace content padding) | 24px | 6x |
| Workspace horizontal padding | 28px | 7x |

**Rule:** If you're writing a spacing value that isn't a multiple of 4, it's wrong.

## Border Radius

**6px. Everywhere. No exceptions.**

Buttons: 6px. Inputs: 6px. Dropdowns: 6px. Badges: 6px. Cards: 6px. Panels: 6px.

The only exception is the app window itself (8px, controlled by the OS/Tauri) and small indicators like connection dots (50% for circles).

## Colors

### Accent Colors (Same in Both Themes)

| Token | Value | Use |
|-------|-------|-----|
| `--primary` | `#4361ee` | Primary buttons, active states, progress bar, chevron prefix |
| `--primary-hover` | `#3a56d4` | Hover state for primary buttons |
| `--success` | theme-specific | Success states, connected indicator, OKAY messages |
| `--warning` | theme-specific | Warning states, EDL mode button |
| `--danger` | `#e74c3c` / `#dc2626` | Error states, destructive actions |

### Surface Colors

Surfaces layer on top of each other. Darker = deeper in the visual hierarchy.

**Dark theme layering:**
```
--bg (#1e2233)          ← workspace background
  --surface (#161b2e)   ← sidebar, log panel, title bar
    --input-bg (rgba white 0.04) ← inputs, inside surface
```

**Light theme layering:**
```
--bg (#ffffff)           ← workspace background
  --surface (#f6f8fa)    ← sidebar, log panel, title bar
    --input-bg (#ffffff)  ← inputs, inside surface (back to white)
```

### Border Colors

Never use hard hex borders. Always rgba overlays:
- Dark: `rgba(255, 255, 255, 0.06)` default, `rgba(255, 255, 255, 0.12)` for emphasis
- Light: `rgba(0, 0, 0, 0.08)` default, `rgba(0, 0, 0, 0.12)` for emphasis

### Text Colors

| Token | Dark | Light | Use |
|-------|------|-------|-----|
| `--text` | `#e5e7eb` | `#24292f` | Primary text, headings |
| `--text-secondary` | `#d1d5db` | `#57606a` | Body text, descriptions |
| `--text-muted` | `rgba(w, 0.4)` | `rgba(b, 0.4)` | Timestamps, hints |
| `--text-label` | `rgba(w, 0.35)` | `rgba(b, 0.4)` | Section headers |

## Themes

### Implementation

- Theme is set via `data-theme` attribute on `<html>` element
- All colors reference CSS custom properties (variables)
- Toggle persisted to `localStorage`
- Default: dark

### Complete Variable Reference

See `docs/superpowers/specs/2026-03-26-ui-redesign-design.md` for the full CSS variable blocks for both themes.

### Rules for New Variables

If you need a new color:
1. Can it be expressed as an existing variable? Use that.
2. Can it be expressed as an rgba overlay on an existing variable? Do that.
3. Must it be a new token? Add it to BOTH theme blocks with the same name. Document it here.

## Components

### Buttons

**Primary:**
```css
background: var(--primary);
color: white;
border: none;
border-radius: 6px;
padding: 9px 16px;      /* or 10px 28px for large */
font-size: 12px;         /* or 13px for large */
font-weight: 600;
```

**Secondary:**
```css
background: var(--button-secondary);
color: var(--text-secondary);
border: 1px solid var(--border-strong);
border-radius: 6px;
padding: 7px 12px;
font-size: 12px;
font-weight: 500;
```

**Sidebar action (reboot buttons):**
```css
background: var(--input-bg);
border: 1px solid var(--border);
border-radius: 6px;
padding: 7px 12px;
font-size: 12px;
color: var(--text-secondary);
```

**Warning variant (EDL):**
Same as sidebar action but with warning-tinted border and text color.

### Inputs

```css
background: var(--input-bg);
border: 1px solid var(--border-strong);
border-radius: 6px;
padding: 9px 12px;
font-size: 12px;
color: var(--text);
```

File path inputs use monospace font. All other inputs use Inter.

### Progress Bar

```css
height: 4px;
background: var(--input-bg);
border-radius: 2px;
/* Fill */
background: var(--primary);
border-radius: 2px;
```

2px radius on the progress bar is the ONE exception to 6px — because 6px on a 4px-tall element looks wrong.

### Badges (Mode Indicator)

```css
display: inline-flex;
align-items: center;
gap: 8px;
background: rgba(67, 97, 238, 0.1);
border: 1px solid rgba(67, 97, 238, 0.2);
border-radius: 6px;
padding: 6px 12px;
font-size: 12px;
font-weight: 600;
color: /* primary shade per theme */;
```

### Connection Dot

```css
width: 6px;
height: 6px;
border-radius: 50%;
background: var(--success);
box-shadow: 0 0 6px rgba(success, 0.4);
```

## Icons

- No emoji in the UI. Use SVG icons or no icons.
- Icon size: 16px default, 12px in tight spaces
- Icon color: `currentColor` (inherits from text)
- App logo: geometric hexagon in primary blue (see title bar)

## Transitions

```css
transition: background 0.15s, color 0.15s, border-color 0.15s;
```

- 0.15s for hover/focus states
- 0.3s for progress bar fill
- No transitions on layout changes
- No animations except progress bar indeterminate

## Hover States

- Buttons: darken background slightly
- Primary buttons: use `--primary-hover`
- Secondary/ghost buttons: use `--surface-hover`
- Never change border-radius, padding, or font on hover
- Never use `transform` on hover (no translateY, no scale)

## What NOT To Do

These are banned. If you find yourself reaching for any of these, stop.

- **Gradients** on surfaces or backgrounds
- **Box shadows** on components (only on the app window itself)
- **Emoji** as icons (use SVG or nothing)
- **Bold (700)** font weight
- **Arbitrary colors** not in the variable system
- **Arbitrary spacing** not on the 4px grid
- **Arbitrary border-radius** (it's 6px or it's wrong)
- **Arbitrary font sizes** (use the type scale)
- **`translateY` on hover** (no floating buttons)
- **Multiple accent colors** (#4361ee is the only accent)
- **Rounded pill shapes** (border-radius: 999px)
- **Opacity for disabled** below 0.4 (use exactly 0.4)
- **Hard hex borders** (use rgba)

## Adding New Components

When creating any new UI element:

1. Check if an existing component pattern covers it (button, input, badge)
2. Use variables from this document — never hardcode colors, sizes, or spacing
3. Follow the typography rules — Inter for UI, monospace for code
4. 6px radius
5. 4px spacing grid
6. Test in BOTH themes

If it doesn't look right following these rules, the rules might need updating — but update the rules FIRST (in this document), then apply everywhere. Never make a one-off exception.
