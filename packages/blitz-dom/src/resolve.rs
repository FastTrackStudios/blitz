//! Resolve style and layout

use blitz_traits::node_id::NodeId;
use std::cell::RefCell;

use debug_timer::debug_timer;
use kurbo::{Affine, Rect};
use parley::LayoutContext;
use selectors::Element as _;
use style::dom::TDocument;
use style::selector_parser::RestyleDamage;
use stylo_taffy::CopyStyleGetters;

#[cfg(feature = "parallel-construct")]
use rayon::prelude::*;

// FIXME: static thread_local FontCtx isn't necessarily correct in multi-document context.
// Should use thread_local crate with ThreadLocal value store in the Document.
thread_local! {
    pub(crate) static LAYOUT_CTX: RefCell<Option<Box<LayoutContext<TextBrush>>>> = const { RefCell::new(None) };
}

use taffy::AvailableSpace;

use crate::{
    BaseDocument,
    layout::{
        LayoutPassState,
        construct::{
            ConstructionTask, ConstructionTaskData, ConstructionTaskResult,
            ConstructionTaskResultData, LayoutChildren, collect_layout_children,
        },
        damage::{
            ALL_DAMAGE, CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC, LAYOUT_UNCHANGED,
            ONLY_RELAYOUT,
        },
        inline::build_inline_layout_into,
    },
    node::TextBrush,
};

impl BaseDocument {
    /// Restyle the tree and then relayout it
    /// [`resolve`](Self::resolve) again at the animation time of the last
    /// resolve: for bringing layout up to date between frames (an input
    /// event arriving after mutations, before the frame that would resolve
    /// them) without moving animations off the window's clock.
    pub fn resolve_at_last_time(&mut self) {
        self.resolve(self.last_animation_time);
    }

    pub fn resolve(&mut self, current_time_for_animations: f64) {
        self.last_animation_time = current_time_for_animations;
        if TDocument::as_node(&self.root_node())
            .first_element_child()
            .is_none()
        {
            #[cfg(feature = "tracing")]
            tracing::warn!("No DOM - not resolving");
            return;
        }

        // Process messages that have been sent to our message channel (e.g. loaded resource)
        self.handle_messages();

        // While render-blocking resources (e.g. stylesheets linked from the `<head>`) are
        // still loading, don't resolve styles or layout (matching how browsers block
        // rendering). Resolving styles before the document's stylesheets have loaded would
        // give elements computed styles based on an incomplete cascade, and a later restyle
        // (once the stylesheet loads) would treat those as genuine "before-change styles",
        // spuriously starting CSS transitions from unstyled values. See issue #689.
        //
        // `handle_messages` above must still run so that loaded resources are ingested and
        // this state can clear.
        if self.has_pending_critical_resources() {
            return;
        }

        self.resolve_scroll_animation();

        // Drop scrollbar-activity entries whose fade-out has finished (also
        // sheds entries for removed nodes).
        {
            use crate::node::scrollbar::{FADE_DELAY, FADE_DURATION};
            self.scrollbar_activity
                .retain(|_, last| last.elapsed() < FADE_DELAY + FADE_DURATION);
        }

        let root_node_id = self.root_element().id;
        debug_timer!(timer, feature = "log-phase-times");

        // Apply any device changes (viewport resize, zoom, color-scheme, etc)
        // accumulated since the last resolve as a single device rebuild.
        // FTS: read before the flush clears it (see the layout skip below).
        let device_changed = !self.pending_device_changes.is_empty();
        self.flush_pending_device_changes();

        // we need to resolve stylist first since it will need to drive our layout bits
        self.resolve_stylist(current_time_for_animations);
        timer.record_time("style");

        // Non-incremental mode is "every node is damaged every frame": mark the
        // whole tree and let the same damage pipeline reconstruct and relayout
        // everything.
        if !self.incremental_layout {
            self.mark_all_damaged();
            timer.record_time("mark_all");
        }

        // FTS: frames with nothing to lay out skip layout.
        //
        // - Idle: nothing mutated or restyled. Skip every tree walk and
        //   just paint. A host with custom widgets repaints on their
        //   schedule, and each of those frames used to lay the whole
        //   document out again.
        // - Paint-only: a colour, a shadow, a transform. Styles are
        //   resolved and painting reads them directly; only overflow,
        //   transforms and the paint tree need their pass. Stylo counts
        //   some of these (a box-shadow) as relayout;
        //   `compute_layout_damage` marks the ones whose layout styles are
        //   all unchanged, and the relayout bit comes off them here.
        //
        // A device change (a resize, a zoom) moves the available space
        // without damaging any node, so it always lays out.
        //
        // BLITZ_NO_IDLE_SKIP=1 turns both off.
        let skip_allowed = self.incremental_layout
            && !device_changed
            && std::env::var_os("BLITZ_NO_IDLE_SKIP").is_none();
        let paint_only = RestyleDamage::REPAINT
            | RestyleDamage::REBUILD_STACKING_CONTEXT
            | RestyleDamage::RECALCULATE_OVERFLOW;
        let mut any_damage = false;
        let mut needs_layout = false;
        for (_, node) in self.nodes.iter_mut() {
            let Some(mut damage) = node.damage() else {
                continue;
            };
            if damage.contains(LAYOUT_UNCHANGED) {
                if !damage.intersects(CONSTRUCT_BOX | CONSTRUCT_FC | CONSTRUCT_DESCENDENT) {
                    damage.remove(ONLY_RELAYOUT);
                }
                damage.remove(LAYOUT_UNCHANGED);
                node.set_damage(damage);
            }
            any_damage |= !damage.is_empty();
            needs_layout |= !(damage - paint_only).is_empty();
        }
        self.resolve_damaged = !skip_allowed || any_damage;
        // FTS: a keyframe animation of `transform` restyles its node with no
        // damage stylo reports, so the skip below never resolved its
        // transform again and it held the first frame's (opacity, read at
        // paint, still moved). While anything animates, transforms are
        // resolved each pass.
        if skip_allowed && !any_damage && !self.has_active_animations {
            // Pending style images still flush (a restyle that changed only
            // a background image reports no damage of its own here).
            self.flush_pending_style_images();
            for (_, node) in self.nodes.iter_mut() {
                node.unset_dirty_descendants();
            }
            // A scroll moves no layout, but it moves what sticks.
            if !self.sticky_nodes.is_empty() {
                self.apply_sticky(false);
                self.nodes.bump_geometry_generation();
            }
        } else if skip_allowed && !needs_layout {
            self.propagate_damage_flags(root_node_id);
            timer.record_time("damage");
            self.flush_pending_style_images();
            self.resolve_transforms(root_node_id);
            self.apply_sticky(false);
            timer.record_time("transform");
            self.build_paint_tree(root_node_id);
            timer.record_time("paint_tree");
        } else {
            // Propagate damage flags (from mutation and restyles) up and down the tree
            self.propagate_damage_flags(root_node_id);
            timer.record_time("damage");

            // Fix up tree for layout (insert anonymous blocks as necessary, etc)
            self.resolve_layout_children();
            #[cfg(debug_assertions)]
            self.assert_layout_parents_consistent();
            timer.record_time("construct");

            self.resolve_deferred_tasks();
            // Flush background/mask images from style to dedicated storage on the
            // nodes whose style changed (queued by the style traversal and by
            // pseudo-element box construction), fetching any not-yet-loaded images.
            self.flush_pending_style_images();
            timer.record_time("pconstruct");

            // Next we resolve layout with the data resolved by stlist
            self.resolve_layout();
            timer.record_time("layout");
            self.apply_sticky(true);
            self.nodes.bump_geometry_generation();

            // Resolve transforms
            self.resolve_transforms(root_node_id);
            timer.record_time("transform");

            // Build paint children / stacking contexts for damaged subtrees. Runs
            // after layout and transforms so that stacking-context roots created by
            // this frame's transforms are picked up, and before damage is cleared.
            self.build_paint_tree(root_node_id);
            timer.record_time("paint_tree");
        }

        // Clear all damage and dirty flags, walking only subtrees which are
        // marked as (potentially) containing damage.
        let doc_node_id = self.root_node().id;
        self.clear_damage_and_dirty_flags(doc_node_id);
        timer.record_time("c_damage");

        // Re-resolve the hover node from the pointer position against the fresh
        // layout. This must run *after* the damage/dirty flags are cleared
        // above, so that the restyle hint and ancestor `dirty_descendants`
        // flags set by any resulting hover change survive into the next resolve
        // pass (the clearing loop would otherwise wipe them). Any resulting
        // restyle is picked up on the next resolve pass; a redraw is requested
        // if the hovered node actually changes.
        self.refresh_hover();

        let mut subdoc_is_animating = false;
        for &node_id in &self.sub_document_nodes {
            let node = &mut self.nodes[node_id];
            let size = node.final_layout().size;
            if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
                // Set viewport
                // viewport_mut handles change detection. So we just unconditionally set the values;
                let mut sub_viewport = sub_doc.viewport_mut();
                sub_viewport.hidpi_scale = self.viewport.hidpi_scale;
                sub_viewport.zoom = self.viewport.zoom;
                sub_viewport.color_scheme = self.viewport.color_scheme;

                let viewport_scale = self.viewport.scale();
                sub_viewport.window_size = (
                    (size.width * viewport_scale) as u32,
                    (size.height * viewport_scale) as u32,
                );
                drop(sub_viewport);

                sub_doc.resolve(current_time_for_animations);

                subdoc_is_animating |= sub_doc.is_animating();
            }
        }
        self.subdoc_is_animating = subdoc_is_animating;
        timer.record_time("subdocs");

        timer.print_times(&format!("Resolve({}): ", self.id()));
    }

    fn resolve_transforms(&mut self, node_id: NodeId) -> Rect {
        if !self.nodes.contains_key(node_id) {
            return Rect::ZERO;
        }

        let scale = self.viewport.scale_f64();

        if !self.nodes[node_id]
            .damage()
            .map(|d| d.contains(style::selector_parser::RestyleDamage::RECALCULATE_OVERFLOW))
            .unwrap_or(false)
        {
            let node = &self.nodes[node_id];
            let location = node.final_layout().location.map(|v| v as f64 * scale);

            let mut transform = Affine::translate((location.x, location.y));
            if let Some(t) = node.transform().as_deref() {
                transform *= *t
            }

            let overflow = *node.scrollable_overflow();
            return transform.transform_rect_bbox(overflow);
        }

        let transform = self.nodes[node_id].set_transform(scale as f32);

        let w = self.nodes[node_id].final_layout().size.width as f64 * scale;
        let h = self.nodes[node_id].final_layout().size.height as f64 * scale;
        let mut overflow = Rect::new(0.0, 0.0, w, h);

        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());

        if let Some(ref children) = layout_children {
            for &child_id in children {
                // Out-of-flow children are laid out relative to their containing
                // block, not their DOM parent: they are visited (and their overflow
                // accounted for) via the containing block's hoisted list below.
                if self.nodes[child_id].is_out_of_flow() {
                    continue;
                }
                let child_rect_in_self = self.resolve_transforms(child_id);
                overflow = overflow.union(child_rect_in_self);
            }
        }
        let hoisted_children =
            std::mem::take(&mut *self.nodes[node_id].hoisted_children.borrow_mut());
        for &child_id in &hoisted_children {
            if !self.nodes.contains_key(child_id) {
                continue;
            }
            let child_rect_in_self = self.resolve_transforms(child_id);
            overflow = overflow.union(child_rect_in_self);
        }
        *self.nodes[node_id].hoisted_children.borrow_mut() = hoisted_children;
        for pseudo in [self.nodes[node_id].before(), self.nodes[node_id].after()] {
            let Some(pseudo) = pseudo else { continue };
            if self.nodes[pseudo].is_out_of_flow() {
                continue;
            }
            let child_rect_in_self = self.resolve_transforms(pseudo);
            overflow = overflow.union(child_rect_in_self);
        }

        *self.nodes[node_id].scrollable_overflow_mut() = overflow;
        *self.nodes[node_id].layout_children.get_mut() = layout_children;

        let scaled_x = self.nodes[node_id].final_layout().location.x as f64 * scale;
        let scaled_y = self.nodes[node_id].final_layout().location.y as f64 * scale;

        let full = if let Some(t) = transform {
            Affine::translate((scaled_x, scaled_y)) * t
        } else {
            Affine::translate((scaled_x, scaled_y))
        };

        full.transform_rect_bbox(overflow)
    }

    /// Ensure that the layout_children field is populated for all nodes
    pub fn resolve_layout_children(&mut self) {
        resolve_layout_children_recursive(self, self.root_node().id);

        fn resolve_layout_children_recursive(doc: &mut BaseDocument, node_id: NodeId) {
            // Anonymous blocks and pseudo-elements can be removed from the slab
            // between render passes. Bail out rather than panicking on a stale key.
            if doc.nodes.get(node_id).is_none() {
                return;
            }

            let mut damage = doc.nodes[node_id].damage().unwrap_or(ALL_DAMAGE);
            let _flags = doc.nodes[node_id].flags;

            if damage.intersects(CONSTRUCT_FC | CONSTRUCT_BOX) {
                //} || flags.contains(NodeFlags::IS_INLINE_ROOT) {

                // Deallocate the anonymous blocks created for this node in the
                // previous construction round. They live only in the slab, so
                // reconstructing without freeing them would leak a slab entry per
                // anonymous block per reconstruction.
                let old_anonymous_blocks = std::mem::take(&mut doc.nodes[node_id].anonymous_blocks);
                for anon_id in old_anonymous_blocks {
                    doc.deallocate_anonymous_block(anon_id);
                }

                let mut collected = LayoutChildren::default();
                collect_layout_children(doc, node_id, &mut collected);
                let layout_children = collected.children;
                doc.nodes[node_id].anonymous_blocks = collected.anonymous_blocks;

                // Recurse into newly collected layout children
                for child_id in layout_children.iter().copied() {
                    resolve_layout_children_recursive(doc, child_id);
                    doc.nodes[child_id].layout_parent.set(Some(node_id));
                    if let Some(mut data) = doc.nodes[child_id]
                        .try_stylo_element_data_mut()
                        .and_then(|s| s.get_mut())
                    {
                        data.damage
                            .remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                }

                *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children.clone());
                // *doc.nodes[node_id].paint_children.borrow_mut() = Some(layout_children);

                damage.remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            } else {
                //if damage.contains(CONSTRUCT_DESCENDENT) {
                let layout_children = doc.nodes[node_id].layout_children.borrow_mut().take();
                if let Some(layout_children) = layout_children {
                    for child_id in layout_children.iter().copied() {
                        // Anonymous blocks and pseudo-elements can be removed from the
                        // slab between render passes; skip stale IDs.
                        if !doc.nodes.contains_key(child_id) {
                            continue;
                        }
                        resolve_layout_children_recursive(doc, child_id);
                        doc.nodes[child_id].layout_parent.set(Some(node_id));
                    }

                    *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children);
                }

                // damage.remove(CONSTRUCT_DESCENDENT);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            }

            doc.nodes[node_id].set_damage(damage);
        }
    }

    /// Every layout child must point back at its container via
    /// `layout_parent`, which `propagate_damage_flags` relies on to reach
    /// anonymous boxes.
    #[cfg(debug_assertions)]
    fn assert_layout_parents_consistent(&self) {
        use crate::node::NodeFlags;

        for (parent_id, node) in self.nodes.iter() {
            if !node.flags.contains(NodeFlags::IS_IN_DOCUMENT) {
                continue;
            }
            let Some(children) = node.layout_children.borrow().clone() else {
                continue;
            };
            for child_id in children {
                let Some(child) = self.nodes.get(child_id) else {
                    panic!("layout child {child_id:?} of {parent_id:?} is not in the slab");
                };
                debug_assert_eq!(
                    child.layout_parent.get(),
                    Some(parent_id),
                    "layout_parent of {child_id:?} does not point at its layout container {parent_id:?}"
                );
            }
        }
    }

    pub fn resolve_deferred_tasks(&mut self) {
        let mut deferred_construction_nodes = std::mem::take(&mut self.deferred_construction_nodes);

        // Deduplicate deferred tasks by node_id to avoid redundant work
        deferred_construction_nodes.sort_unstable_by_key(|task| task.node_id);
        deferred_construction_nodes.dedup_by_key(|task| task.node_id);

        #[cfg(feature = "parallel-construct")]
        let iter = deferred_construction_nodes.into_par_iter();
        #[cfg(not(feature = "parallel-construct"))]
        let iter = deferred_construction_nodes.into_iter();

        let results: Vec<ConstructionTaskResult> = iter
            .map(|task: ConstructionTask| match task.data {
                ConstructionTaskData::InlineLayout(mut layout) => {
                    #[cfg(feature = "parallel-construct")]
                    let mut layout_ctx = LAYOUT_CTX
                        .take()
                        .unwrap_or_else(|| Box::new(LayoutContext::new()));
                    #[cfg(feature = "parallel-construct")]
                    let layout_ctx_mut = &mut layout_ctx;

                    #[cfg(feature = "parallel-construct")]
                    let mut font_ctx = self
                        .thread_font_contexts
                        .get_or(|| RefCell::new(Box::new(self.font_ctx.lock().unwrap().clone())))
                        .borrow_mut();
                    #[cfg(feature = "parallel-construct")]
                    let font_ctx_mut = &mut *font_ctx;

                    #[cfg(not(feature = "parallel-construct"))]
                    let layout_ctx_mut = &mut self.layout_ctx;
                    #[cfg(not(feature = "parallel-construct"))]
                    let font_ctx_mut = &mut *self.font_ctx.lock().unwrap();

                    layout.content_widths = None;
                    build_inline_layout_into(
                        &self.nodes,
                        layout_ctx_mut,
                        font_ctx_mut,
                        &mut layout,
                        self.viewport.scale(),
                        task.node_id,
                    );

                    #[cfg(feature = "parallel-construct")]
                    {
                        LAYOUT_CTX.set(Some(layout_ctx));
                    }

                    // If layout doesn't contain any inline boxes, then it is safe to populate the content_widths
                    // cache during this parallelized stage.
                    // if layout.layout.inline_boxes().is_empty() {
                    //     layout.content_widths();
                    // }

                    ConstructionTaskResult {
                        node_id: task.node_id,
                        data: ConstructionTaskResultData::InlineLayout(layout),
                    }
                }
            })
            .collect();

        for result in results {
            match result.data {
                ConstructionTaskResultData::InlineLayout(layout) => {
                    self.nodes[result.node_id].clear_layout_cache();
                    self.nodes[result.node_id]
                        .element_data_mut()
                        .unwrap()
                        .inline_layout_data = Some(layout);
                }
            }
        }

        self.deferred_construction_nodes.clear();
    }

    /// Walk the nodes now that they're properly styled and transfer their styles to the taffy style system
    ///
    /// TODO: update taffy to use an associated type instead of slab key
    /// TODO: update taffy to support traited styles so we don't even need to rely on taffy for storage
    /// FTS: `position: sticky` (top only). Layout places a sticky node as
    /// a relative one; this moves it down so it stays `top` below the top
    /// of its nearest vertically scrolling ancestor while that is scrolled
    /// past it — never out of its parent. The move is written into
    /// `final_layout.location.y` (and remembered in `sticky_shift`), so
    /// painting and hit testing see it. `fresh`: layout just ran, so the
    /// locations are layout's own.
    pub(crate) fn apply_sticky(&mut self, fresh: bool) {
        use style::values::computed::Overflow;
        if fresh {
            self.sticky_nodes = self
                .nodes
                .iter()
                .filter(|(_, n)| {
                    n.primary_styles().is_some_and(|s| {
                        matches!(
                            s.clone_position(),
                            style::computed_values::position::T::Sticky
                        )
                    })
                })
                .map(|(id, _)| id)
                .collect();
            for &id in &self.sticky_nodes {
                self.nodes[id].layout_data_mut().sticky_shift = 0.0;
            }
        }
        // Outermost first: a sticky node inside a sticky one moves with it.
        let depth = |doc: &Self, id: NodeId| {
            let mut n = 0usize;
            let mut at = doc.nodes.get(id).and_then(|n| n.layout_parent.get());
            while let Some(p) = at {
                n += 1;
                at = doc.nodes.get(p).and_then(|n| n.layout_parent.get());
            }
            n
        };
        let mut order: Vec<(usize, NodeId)> = self
            .sticky_nodes
            .iter()
            .map(|&id| (depth(self, id), id))
            .collect();
        order.sort_unstable_by_key(|(depth, _)| *depth);
        for (_, id) in order {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            let top = {
                let Some(styles) = node.primary_styles() else {
                    continue;
                };
                stylo_taffy::convert::inset(&styles.get_position().top)
                    .resolve_to_option(0.0, |_, _| 0.0)
            };
            let Some(top) = top else { continue };
            // Where layout put it, which is where it sits unstuck: Taffy
            // places `Position::Sticky` at its static position (it used to
            // be converted to `Relative`, which moved it down by `top`).
            let laid = node.final_layout().location.y - node.layout_data().sticky_shift;
            let base = laid;
            let height = node.final_layout().size.height;
            let Some(parent) = node.layout_parent.get() else {
                continue;
            };
            // Up to the scroller: the parent's top in its content, and the
            // parent's height (what the node must stay inside).
            let parent_h = self
                .nodes
                .get(parent)
                .map_or(0.0, |p| p.final_layout().size.height);
            let mut parent_y = 0.0f32;
            let mut scroller = None;
            let mut at = Some(parent);
            while let Some(a) = at {
                let Some(n) = self.nodes.get(a) else { break };
                let scrolls = n.primary_styles().is_some_and(|s| {
                    matches!(s.clone_overflow_y(), Overflow::Scroll | Overflow::Auto)
                });
                if scrolls {
                    scroller = Some(a);
                    break;
                }
                parent_y += n.final_layout().location.y;
                // Locations are relative to the containing block (an
                // out-of-flow box's is not its layout parent).
                at = n.containing_block();
            }
            let Some(scroller) = scroller else { continue };
            let scroll = self.nodes[scroller].scroll_offset().y as f32;
            let at_y = parent_y + base;
            let want = at_y.max(scroll + top);
            let shift = (want.min(parent_y + parent_h - height) - at_y).max(0.0);
            let node = &mut self.nodes[id];
            node.final_layout_mut().location.y = base + shift;
            node.layout_data_mut().sticky_shift = base + shift - laid;
        }
    }

    pub fn resolve_layout(&mut self) {
        let size = self.stylist.device().au_viewport_size();

        let available_space = taffy::Size {
            width: AvailableSpace::Definite(size.width.to_f32_px()),
            height: AvailableSpace::Definite(size.height.to_f32_px()),
        };

        let root_element_id = crate::taffy_node_id(self.root_element().id);

        // println!("\n\nRESOLVE LAYOUT\n===========\n");

        let mut state = LayoutPassState::new(self);
        #[cfg(feature = "writing-mode")]
        let available_space = {
            state.layout_wm = state.layout_wm_of(crate::dom_node_id(root_element_id));
            if state.layout_wm.is_vertical() {
                available_space.transpose()
            } else {
                available_space
            }
        };
        taffy::compute_root_layout(&mut state, root_element_id, available_space);
        // Rounding walks the whole tree and is 6% of a frame on a big
        // one, but it cannot be skipped on the grounds that nothing was
        // damaged: `compute_root_layout` rewrites unrounded layouts for
        // anything its cache missed, and `final_layout` is only ever
        // written here. Skipping it left the window laying out at zero
        // size. Making this incremental means teaching Taffy to report
        // which nodes it actually recomputed.
        #[cfg(feature = "writing-mode")]
        state.physicalise_and_round_layout(root_element_id);
        #[cfg(not(feature = "writing-mode"))]
        taffy::round_layout(&mut state, root_element_id);

        // println!("\n\n");
        // taffy::print_tree(self, root_node_id)
    }
}
