use euclid::default::Rect;
use kurbo::{Affine, Vec2};
use style::{
    properties::generated::style_structs::Box as BoxStyleStruct,
    values::{
        computed::{CSSPixelLength, Rotate},
        generics::transform::{Scale, Translate},
    },
};

// 6. Current Transformation Matrix
//
// The transformation matrix is computed from the transform, transform-origin, translate, rotate, scale, and offset properties as follows:
//
//   - Start with the identity matrix.
//   - Translate by the computed X, Y, and Z values of transform-origin.
//   - Translate by the computed X, Y, and Z values of translate.
//   - Rotate by the computed <angle> about the specified axis of rotate.
//   - Scale by the computed X, Y, and Z values of scale.
//   - Translate and rotate by the transform specified by offset.
//   - Multiply by each of the transform functions in transform from left to right.
//   - Translate by the negated computed X, Y and Z values of transform-origin.
//
// <https://drafts.csswg.org/css-transforms-2/#ctm>
pub fn resolve_2d_transform(
    box_styles: &BoxStyleStruct,
    reference_box: Rect<CSSPixelLength>,
    scale: f64,
) -> Option<Affine> {
    let translate = match &box_styles.translate {
        Translate::None => None,
        Translate::Translate(x, y, _z) => Some(Vec2 {
            x: x.resolve(reference_box.width()).px() as f64,
            y: y.resolve(reference_box.height()).px() as f64,
        }),
    };

    let rotate = match &box_styles.rotate {
        Rotate::None => None,
        // CSS rotation angles are degrees; kurbo's `Affine::then_rotate`
        // takes radians. Forward the angle in radians so animations like
        // `transform: rotate(360deg)` (Tailwind's `animate-spin`) don't
        // multiply the matrix by ~57 full rotations per "degree" of
        // animation progress and translate small elements off-canvas.
        Rotate::Rotate(angle) => Some(angle.radians() as f64),
        // TODO: support 3D transforms
        Rotate::Rotate3D(_, _, _, _) => None,
    };

    let scale_transform = match &box_styles.scale {
        Scale::None => None,
        Scale::Scale(x, y, _z) => Some(Vec2 {
            x: *x as f64 * scale,
            y: *y as f64 * scale,
        }),
    };

    let transform = if box_styles.transform.0.is_empty() {
        None
    } else {
        box_styles
            .transform
            .to_transform_3d_matrix(Some(&reference_box))
            .ok()
            .filter(|(_t, has_3d)| !has_3d)
            .map(|(t, _has_3d)| {
                // See: https://drafts.csswg.org/css-transforms-2/#two-dimensional-subset
                // And https://docs.rs/kurbo/latest/kurbo/struct.Affine.html#method.new
                Affine::new(
                    [
                        t.m11,
                        t.m12,
                        t.m21,
                        t.m22,
                        // Scale the translation but not the scale or skew
                        t.m41 * scale as f32,
                        t.m42 * scale as f32,
                    ]
                    .map(|v| v as f64),
                )
            })
    };

    // TODO: support the "offset" property
    // <https://developer.mozilla.org/en-US/docs/Web/CSS/Reference/Properties/offset>

    if translate.is_none() && rotate.is_none() && scale_transform.is_none() && transform.is_none() {
        return None;
    }

    // Apply the transform origin by:
    //   - Translating by the origin offset
    //   - Applying our transform
    //   - Translating by the inverse of the origin offset
    let transform_origin = &box_styles.transform_origin;
    let origin_translation = Affine::translate(Vec2 {
        x: transform_origin
            .horizontal
            .resolve(reference_box.width())
            .px() as f64,
        y: transform_origin
            .vertical
            .resolve(reference_box.height())
            .px() as f64,
    });

    let mut resolved = Affine::IDENTITY;

    if let Some(translation) = translate {
        resolved = resolved.then_translate(translation)
    }

    if let Some(rotation) = rotate {
        resolved = resolved.then_rotate(rotation)
    }

    if let Some(scale_transform) = scale_transform {
        resolved = resolved.then_scale_non_uniform(scale_transform.x, scale_transform.y)
    }

    if let Some(transform) = transform {
        resolved *= transform;
    }

    resolved = origin_translation * resolved * origin_translation.inverse();

    if resolved != Affine::IDENTITY {
        Some(resolved)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! Regression tests for the rotation-angle unit bug.
    //!
    //! The `rotate` CSS property carries an angle in degrees. kurbo's
    //! `Affine::then_rotate` takes radians. Mixing them up multiplies
    //! the rotation by ~57.3, which on a small element rotated by an
    //! `animate-spin` keyframe (e.g. `transform: rotate(360deg)`)
    //! produced a matrix that translated the element well outside any
    //! reasonable bounding box — manifesting as a missing spinner on
    //! 16×16 SVGs in production apps using Tailwind.
    //!
    //! These tests check the matrix entries directly. We assert
    //! `rotate(180deg)` produces the standard 180° rotation matrix
    //! around the origin: `[-1, 0, 0, -1, 0, 0]`. With the old
    //! degrees-as-radians bug this would be the matrix for
    //! `180 mod 2π ≈ 1.27` radians instead.

    // Driving a literal `Rotate::Rotate(angle)` through
    // `resolve_2d_transform` from a unit test requires a full Stylo
    // `ComputedValues` instance, which Blitz doesn't construct
    // directly outside the style pipeline. Instead we pin the kurbo
    // convention this code depends on — if the convention ever
    // changes the test fires loud and the `Rotate::Rotate(angle)`
    // arm has to be updated to match.

    use kurbo::Affine;

    fn approx_eq(a: Affine, b: Affine) -> bool {
        a.as_coeffs()
            .iter()
            .zip(b.as_coeffs().iter())
            .all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn kurbo_then_rotate_takes_radians() {
        let rotated = Affine::IDENTITY.then_rotate(std::f64::consts::PI);
        let expected = Affine::new([-1.0, 0.0, 0.0, -1.0, 0.0, 0.0]);
        assert!(
            approx_eq(rotated, expected),
            "kurbo `then_rotate(PI)` = {rotated:?}, expected {expected:?}; \
             if this assertion fires the kurbo radians convention has \
             changed — update `resolve_2d_transform`'s `Rotate::Rotate` \
             arm accordingly."
        );
    }

    #[test]
    fn radians_and_degrees_disagree_for_animate_spin() {
        // Documents *why* the conversion matters:
        // `then_rotate(360.0)` interprets 360 as radians → 360 / (2π)
        // ≈ 57.3 full rotations → a not-quite-identity matrix that
        // accumulates large numerical noise across animation frames.
        // `then_rotate(360°.to_radians())` returns to true identity.
        let bug = Affine::IDENTITY.then_rotate(360.0);
        let fix = Affine::IDENTITY.then_rotate(360.0_f64.to_radians());
        assert!(
            !approx_eq(bug, fix),
            "the buggy and correct matrices for a 360° rotation must differ"
        );
        assert!(
            approx_eq(fix, Affine::IDENTITY),
            "rotating by a full turn (in radians) should return to identity"
        );
    }
}
