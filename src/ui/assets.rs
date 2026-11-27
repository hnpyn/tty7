use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

const STOCK_PREFIX: &str = "stock/";

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(downstream) = path.strip_prefix(STOCK_PREFIX) {
            return gpui_component_assets::Assets.load(downstream);
        }
        if let Some(bytes) = bundled_icon(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}

/// Art this window draws itself, in place of gpui-component's stock glyph.
///
/// `IconName::Plus` and the composer paths `icons/file.svg` /
/// `icons/image.svg` load through here. `image.svg` is not in the stock set,
/// so dropping it would make that path resolve to nothing. Every other name
/// falls through to stock.
fn bundled_icon(path: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match path {
        "icons/plus.svg" => include_bytes!("../../assets/icons/plus.svg"),
        "icons/file.svg" => include_bytes!("../../assets/icons/file.svg"),
        "icons/image.svg" => include_bytes!("../../assets/icons/image.svg"),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_prefix_bypasses_the_overrides() {
        for name in ["plus", "file"] {
            let overridden = Assets
                .load(&format!("icons/{name}.svg"))
                .unwrap()
                .expect("tty7 override present");
            let stock = Assets
                .load(&format!("{STOCK_PREFIX}icons/{name}.svg"))
                .unwrap()
                .expect("downstream glyph present");
            assert_ne!(
                overridden, stock,
                "`{name}` should resolve to different art with and without `{STOCK_PREFIX}`"
            );
        }
    }

    fn glyph(name: &str) -> String {
        let bytes = Assets
            .load(&format!("icons/{name}.svg"))
            .unwrap()
            .unwrap_or_else(|| panic!("nothing serves `icons/{name}.svg`"));
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn attr<'a>(svg: &'a str, name: &str) -> &'a str {
        svg.split_once(&format!("{name}=\""))
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(value, _)| value)
            .unwrap_or_else(|| panic!("no `{name}` in: {svg}"))
    }

    #[test]
    fn the_plus_is_drawn_on_the_bound_the_rest_of_the_set_uses() {
        // Two bare strokes fail quietly. A tightened `plus` does not look
        // broken, it just reads a size smaller than the tiles beside it — which
        // is how it spent a while being scaled up at the call site instead. The
        // icons tty7 draws itself put 19.3 units of ink in a 24 viewBox (a 17.2
        // shape straddled by the family's stroke). A cross gets to sit a little
        // inside that, but not at the 14.1 it had, and not at stock lucide's 14
        // — so a re-tightened path or a re-sync from upstream lands here rather
        // than in someone's peripheral vision.
        let plus = glyph("plus");
        let stroke: f32 = attr(&plus, "stroke-width").parse().unwrap();

        // Each arm is `M<x> <y><axis><len>`: the vertical first, then the
        // horizontal. Both are checked, because a cross edited on one axis
        // only is exactly the kind of miss that survives a glance.
        let arms: Vec<&str> = plus
            .split("d=\"")
            .skip(1)
            .map(|rest| rest.split_once('"').expect("unterminated `d`").0)
            .collect();
        let arm = |d: &str| -> (char, f32, f32, f32) {
            let axis = d
                .chars()
                .find(|c| matches!(c, 'v' | 'h'))
                .unwrap_or_else(|| panic!("plus.svg's `{d}` is neither a `v` nor an `h` run"));
            let (from, len) = d.trim_start_matches('M').split_once(axis).unwrap();
            let (x, y) = from.split_once(' ').unwrap();
            (
                axis,
                x.parse().unwrap(),
                y.parse().unwrap(),
                len.parse().unwrap(),
            )
        };
        assert_eq!(arms.len(), 2, "plus.svg is not two paths: {plus}");
        let (v_axis, v_x, v_top, v_len) = arm(arms[0]);
        let (h_axis, h_left, h_y, h_len) = arm(arms[1]);
        assert_eq!(
            (v_axis, h_axis),
            ('v', 'h'),
            "plus.svg should be a vertical arm then a horizontal one: {plus}"
        );

        for (label, across, along, len) in [
            ("vertical", v_x, v_top, v_len),
            ("horizontal", h_y, h_left, h_len),
        ] {
            assert!(
                (across - 12.).abs() < 0.01 && (along + len / 2. - 12.).abs() < 0.01,
                "plus.svg's {label} arm runs {along}..{} at {across} and so is not \
                 centred in the viewBox",
                along + len
            );
        }
        assert!(
            (v_len - h_len).abs() < 0.01,
            "plus.svg's arms are {v_len} and {h_len} long, so it is not square"
        );

        // Round caps reach `stroke/2` past each end, so what the eye measures
        // is the arm plus one whole stroke — and that total, not the path, is
        // what has to sit inside the set's 19.3.
        let ink = v_len + stroke;
        assert!(
            ink / 19.3 >= 0.85,
            "plus.svg puts {ink} units of ink in the box where the rest of the \
             set puts 19.3, so it will read a size small beside them"
        );

        // One *weight* for the set is not one *density*. The cross is two
        // hairlines and nothing else — about 22 units of stroke — where the
        // closed glyph beside it in the toolbar spends 54 on a perimeter and a
        // divider. Drawn at the family weight the `+` ends up the largest mark
        // in the row and the faintest one at the same time, which is what reads
        // as the odd glyph out. An open form carries a sixth more weight to
        // land at the same optical density; SF Symbols compensates `plus`
        // against `sidebar.left` the same way.
        const OPEN_FORM: f32 = 7. / 6.;
        let family: f32 = attr(&glyph("file"), "stroke-width").parse().unwrap();
        let want = family * OPEN_FORM;
        assert!(
            (stroke - want).abs() < 0.01,
            "plus.svg strokes {stroke} where the set's closed shapes stroke \
             {family}; an open form needs {OPEN_FORM} of that to match them, \
             which is {want}"
        );

        // Extent and weight are only two thirds of it; the last is landing on
        // the pixel grid. The set's other glyphs are closed shapes carrying
        // solid fills, so a soft edge costs them little — a cross is two
        // hairlines and nothing else, and an arm end that straddles pixel rows
        // turns the tip into a smudge. A round cap reaches stroke/2 past the
        // path, so that is where the grid has to be met.
        //
        // Half a CSS pixel, not a whole one, because of what the whole-pixel
        // rungs cost. The tip sits at `k * TILE_GLYPH / 24` for the `k` the
        // path is drawn on, which quantises the cross's *visual* diameter —
        // arm plus one stroke, whatever the weight — to 10px, 12px or 14px in
        // a 16px box. 12 is a size too big beside the closed glyph next to it
        // and 10 is a size too small; there is no third whole-pixel rung to
        // move to. Landing the cap on a half instead buys 11px, which is
        // device-aligned at 2x and only misses the grid at 1x, where a cross
        // this small is already the least of it.
        let tip = (v_top - stroke / 2.) * 16. / 24.;
        let grid = tip * 2.;
        assert!(
            (grid - grid.round()).abs() < 0.01,
            "plus.svg's cap tip lands at {tip} CSS px, which is not even a half \
             pixel"
        );
    }

    #[test]
    fn stock_prefix_works_for_unoverridden_glyphs() {
        assert_eq!(
            Assets.load("stock/icons/check.svg").unwrap(),
            Assets.load("icons/check.svg").unwrap(),
        );
    }
}
