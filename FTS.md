# FastTrackStudios fork notes

This branch is DioxusLabs/blitz `main` plus the FastTrackStudios commits on
top (`git log upstream/main..`). Consumers (architect, studio, task,
keyflow) pin it by tag through their `[patch.crates-io]`; `vendor/anyrender`
and `vendor/anyrender_vello` carry the layered-compositing changes (see
`vendor/anyrender_vello/UPSTREAM.md`).

## Deliberate divergences from upstream's tests

- **An overflow-clipping element roots its z-indexed descendants**
  (`fix(dom): an overflow-clipping element roots its z-indexed descendants`,
  originally 036bf6747). A `position: relative; z-index` child of an
  `overflow: auto/hidden` box is hoisted into that box's stacking context and
  clipped with it, as browsers draw it, instead of escaping to the root's.
  Two upstream tests in `tests/blitz-tests/tests/paint_tree_incremental.rs`
  expected the root to hold the entry and are adjusted to look for it under
  the clipping box: `scroll_moves_hoisted_box_without_resolve` (its
  hit-testing expectations are unchanged) and
  `hoisted_position_is_memoised_per_generation` (the memoisation is checked
  on the clipping box's entry, and holds).
