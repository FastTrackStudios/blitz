# `position: fixed` in the FTS fork — design

Status: **designed, not implemented** (2026-09-21). Motivation: the Signal app
uses Tailwind `fixed inset-0 z-50 …` for every overlay (algorithm pickers,
panel zoom, tuner, command palette, audio settings, click-catchers). Blitz
maps `fixed` to `absolute`, which positions against the immediate parent, so
those overlays render inside their panel and are clipped by its
`overflow: hidden`. One engine fix replaces eight per-overlay workarounds.

Taffy is crates.io 0.12.2, unpatched — the design needs **no Taffy change**.

## Where things are today

- `stylo_taffy/src/convert.rs:223-233` — `Fixed → taffy::Position::Absolute`.
  Keep it: the fixed node's own sizing relies on `Absolute`
  (`inline.rs:757`, `:188`).
- Pipeline (`resolve.rs:42-133`): restyle → `propagate_damage_flags`
  (`damage.rs:34`) → `resolve_layout_children` → `flush_styles_to_layout`
  (`damage.rs:429/520`, walks the whole tree every frame, converts styles,
  builds `paint_children`, hoists `z-index` stacking contexts) →
  `resolve_layout` (`resolve.rs:363`, viewport size from
  `stylist.device().au_viewport_size()`) → `resolve_transforms`.
- Taffy sees `layout_children` through `TraversePartialTree`
  (`layout/mod.rs:284-312`).
- Paint: `paint_scene` (`render.rs:113`) → `render_element` (`:203`) narrows
  clip rects and pushes overflow clip layers; `draw_children` (`:864-909`)
  paints −z hoisted, `paint_children`, +z hoisted. Vello layers nest, so a
  child cannot escape an ancestor's clip — fixed nodes must be drawn from the
  root (which never clips, `:258`).
- Hit test: `Document::hit` (`document.rs:1300`) → `hit_with_scrollbar`
  (`:1435`) → `hit_inner` (`node.rs:1019`), page coordinates.

## Approach

Take fixed nodes **out of Taffy's view of their parent** (so the parent never
sizes or scrolls for them), but leave them in `layout_children` (so damage,
transforms, selection keep working). Lay each fixed subtree out as its own
root against the viewport after the main pass, paint it from the root
stacking context with a viewport-only transform, and hit-test it first.

1. **Flags/storage** — `NodeFlags::IS_FIXED_POS` (`node.rs:55`);
   `Node::taffy_children: RefCell<Option<Vec<usize>>>`; `BaseDocument::
   fixed_nodes: Vec<usize>` (pre-order).
2. **Flush** (`damage.rs`) — set/clear `IS_FIXED_POS` every visit
   (`position == fixed && display != none`); `taffy_children =
   layout_children − fixed`; a fixed child goes into neither `paint_children`
   nor the local stacking context but into `fixed_nodes` and the root's
   stacking context as `HoistedPaintChild { fixed: true, z_index:
   z.integer_or(0) }`; `compute_content_size` skips fixed entries.
3. **Taffy traversal** (`layout/mod.rs:284-312`) — read `taffy_children`
   when `Some`.
4. **Inline contexts** (`inline.rs:298/689`) — a fixed inline box stays 0×0
   and is not laid out there.
5. **Helper** — move `layout_abspos_child` (`inline.rs:742-1047`) to
   `layout/abspos.rs` as `layout_abspos_node(tree, id, static_pos, area_size,
   area_offset, direction)`; inline caller unchanged in behaviour.
6. **`resolve_layout`** — after `compute_root_layout` + `round_layout(root)`,
   for each fixed node in pre-order: static position ≈ parent's document
   position + content-box offset; `layout_abspos_node(…, viewport, …)`;
   `round_layout(id)`. `final_layout.location` is then in viewport space; one
   layout context per subtree keeps the cache sound.
7. **`resolve_transforms`** — recurse into fixed children but do not union
   them into the parent's `scrollable_overflow`.
8. **Paint** (`draw_children`) — a `fixed` hoisted entry renders with
   `Affine::translate((initial_x, initial_y))` (no viewport/ancestor scroll or
   transform) and a viewport clip rect; it escapes every overflow clip.
9. **Hit test** — `hit_inner` gains `fixed_pt: Option<(f32, f32)>` (viewport
   coords, `Some` only from the root); the root tests fixed entries first,
   bypassing `matches_hoisted_content` and the early return at `node.rs:1086`.
10. **Absolute position** — `BaseDocument::absolute_position(id)` stops at an
    `IS_FIXED_POS` node and adds `viewport_scroll`; switch
    `get_client_bounding_rect`, scroll-into-view, sub-document coords.

**Deliberate deviations:** no ancestor-created containing block
(transform/filter/contain); ancestor opacity/filter not applied (could carry
an opacity product on the hoisted entry later); z-order resolved at the root;
approximate static position (Tailwind always sets insets).

**Risks:** damage inside a fixed subtree still invalidates ancestors (correct,
slow — optionally mask RELAYOUT); `taffy_children` must be rebuilt every flush;
stale ids between flush and hit (existing guards cover it). Existing bug
noticed: hoisted positions are summed from `final_layout` during flush, which
runs before layout (one frame stale).

## Tests — `packages/blitz-html/tests/position_fixed.rs`

Fixture: 800×600 viewport, `<div style="position:relative;overflow:hidden;
width:100px;height:100px;margin:50px">` with a fixed child. Patterns from
`paint_order.rs` (pixels via `VelloCpuImageRenderer`) and `pointer_events.rs`.

1. `inset:0` → `final_layout` (0,0) 800×600 and matching client rect.
2. Escapes the parent's clip (pixel at (5,5) is the fixed colour).
3. Unmoved by a scrolled `overflow:auto` parent (pixels and `hit`).
4. Unmoved by viewport scroll; `hit(5,505)` finds it.
5. A backdrop over a `<button>` takes the click; its dialog child is hit.
6. z-order: fixed z50 over later normal content; later absolute z60 over it;
   negative-z fixed under content.
7. A 2000² fixed child does not grow a 100² `overflow:auto` parent or add a
   scrollbar.
8. Fixed span inside a `<p>`: at the viewport origin, text unchanged, no panic.
9. static → fixed → static toggles; viewport resize resizes `inset:0`.
10. Nested fixed; removing a fixed node then `hit` before resolve.
11. Re-run `paint_order`, `pointer_events`, `scrollbars`, `display_contents`.
