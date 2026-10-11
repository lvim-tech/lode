//! Charts: the arithmetic, apart from the drawing.
//!
//! Everything here is a pure function with a test, for the same reason the
//! grid's geometry is: an axis whose ticks are at 0, 3.7142857 and 7.4285714 is
//! a chart nobody can read a value off, and that is not something to discover
//! by looking at a screenshot. The drawing itself is in `app.rs`, where the
//! theme and the window are.
//!
//! What is NOT here: a plotting library. Five kinds of chart over one page of
//! rows need a few hundred lines of arithmetic, and a dependency that draws
//! them would bring its own idea of fonts, colours and interaction — three
//! things this client has already decided.

/// How a measurement is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    /// Horizontal bars with their categories named beside them. The default,
    /// because a result's first column is usually a name rather than a number
    /// and a named bar is the one chart that reads correctly with no axis.
    #[default]
    Bars,
    Line,
    Area,
    Scatter,
    Pie,
}

impl Kind {
    pub fn all() -> [Kind; 5] {
        [Kind::Bars, Kind::Line, Kind::Area, Kind::Scatter, Kind::Pie]
    }

    /// The word stored and sent. Lower case and stable: it goes in the engine's
    /// store, where a reordered enum must not change what was saved.
    pub fn key(self) -> &'static str {
        match self {
            Kind::Bars => "bars",
            Kind::Line => "line",
            Kind::Area => "area",
            Kind::Scatter => "scatter",
            Kind::Pie => "pie",
        }
    }

    /// Back from the stored word. Anything else is `Bars`, because a chart
    /// saved by a NEWER client with a kind this one cannot draw should still
    /// open — as a chart of the right data drawn the plain way, with its kind
    /// visible in the picker, rather than as an error.
    pub fn from_key(s: &str) -> Kind {
        match s.trim().to_ascii_lowercase().as_str() {
            "line" => Kind::Line,
            "area" => Kind::Area,
            "scatter" => Kind::Scatter,
            "pie" => Kind::Pie,
            _ => Kind::Bars,
        }
    }
}

/// How many points are drawn before the rows are sampled.
///
/// Two thousand is well past the point where another point changes the picture
/// on a canvas a thousand pixels wide, and well under the point where building
/// the path costs a frame.
pub const MAX_POINTS: usize = 2000;

/// Which rows to draw, when there are more than `max`.
///
/// Every k-th, and ALWAYS the last one: a line whose final point is missing
/// ends short of the right edge, which reads as a gap in the data rather than
/// as sampling. Returning indices rather than values keeps the labels and the
/// hover pointing at the right row.
pub fn sample(len: usize, max: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    if len <= max || max == 0 {
        return (0..len).collect();
    }
    let step = (len as f64 / max as f64).ceil() as usize;
    let mut out: Vec<usize> = (0..len).step_by(step.max(1)).collect();
    if out.last() != Some(&(len - 1)) {
        out.push(len - 1);
    }
    out
}

/// Where to put the ticks on an axis from `lo` to `hi`.
///
/// At ROUND numbers — 0, 25, 50 — rather than at equal fractions of the range,
/// because an axis labelled 0, 3.714, 7.429 is an axis nobody reads a value
/// off. The step is 1, 2, 5 or 10 times a power of ten, which is what every
/// ruler in the world is marked in.
///
/// The ticks may fall outside `lo..hi`; the caller clips them. That is
/// deliberate: a tick AT the edge is what makes the edge readable, and
/// trimming here would hide whether the axis starts at a round number.
pub fn axis_ticks(lo: f64, hi: f64, want: usize) -> Vec<f64> {
    if !lo.is_finite() || !hi.is_finite() || want == 0 {
        return Vec::new();
    }
    if (hi - lo).abs() < f64::EPSILON {
        // A flat series. One tick, at the value itself: an axis from 7 to 7
        // has one thing to say.
        return vec![lo];
    }
    let (lo, hi) = (lo.min(hi), lo.max(hi));
    let raw = (hi - lo) / want as f64;
    let mag = 10f64.powf(raw.abs().log10().floor());
    // 1, 2, 5, 10 — the steps a ruler has — and the one whose tick COUNT comes
    // closest to what was asked for.
    //
    // Rounding the step up, as the usual recipe does, is what made an axis from
    // 3 to 97 come out with two ticks: 94/4 is 23.5, which rounds up to 50, and
    // an axis with two marks on it is not marked. Too few ticks is as unreadable
    // as odd ones, so the count decides.
    let step = [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .min_by_key(|&cand| {
            let n = ((hi - lo) / cand).floor() as i64;
            (n - want as i64).abs()
        })
        .unwrap_or(mag);
    let first = (lo / step).ceil() * step;
    let mut out = Vec::new();
    let mut v = first;
    // `want + 2` is a guard, not a target: the loop ends at `hi`, and this only
    // stops a rounding error from running it forever.
    while v <= hi + step * 0.5 && out.len() < want + 2 {
        // -0.0 prints as "-0", which is nobody's idea of a tick.
        out.push(if v == 0.0 { 0.0 } else { v });
        v += step;
    }
    out
}

/// A tick's label: short, and without a trailing `.0`.
///
/// An axis is read at a glance, so 1200000 is `1.2M` and 0.5 is `0.5`. The
/// exact value is in the grid; this is the ruler.
pub fn tick_label(v: f64) -> String {
    // Minus zero is zero. Without this it prints as "-0", which is nobody's
    // idea of a tick, and it is what `0.0 - 0.0` and a negated zero both are.
    let v = if v == 0.0 { 0.0 } else { v };
    let a = v.abs();
    let (n, suffix) = if a >= 1e9 {
        (v / 1e9, "G")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e3 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    // At most one decimal, and no `.0`: `1.2M`, `25`, `0.5`.
    let mut s = format!("{:.1}", n);
    if s.ends_with(".0") {
        s.truncate(s.len() - 2);
    }
    format!("{s}{suffix}")
}

/// The slices of a pie, as `(start, sweep)` in TURNS — one turn is the whole
/// circle.
///
/// Turns rather than radians because the drawing wants fractions of a circle
/// and a fraction is easier to be sure about: the sweeps add up to 1.
///
/// NEGATIVE values are dropped rather than drawn: a pie is a whole divided into
/// parts, and a part of minus three is not a share of anything. Nothing is
/// drawn at all when the total is zero, because every slice would be nothing.
pub fn slices(values: &[f64]) -> Vec<(f32, f32)> {
    let total: f64 = values.iter().filter(|v| v.is_finite() && **v > 0.0).sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(values.len());
    let mut at = 0.0f64;
    for v in values {
        let share = if v.is_finite() && *v > 0.0 { v / total } else { 0.0 };
        out.push((at as f32, share as f32));
        at += share;
    }
    out
}

/// Which point the pointer is nearest, along the x axis.
///
/// By x alone: the points of a line are spread along x and stacked on y, so the
/// nearest point to the eye is the nearest in x — and asking for both would
/// mean a tooltip that refuses to appear unless the pointer is on the line
/// itself.
pub fn nearest(x: f32, count: usize, width: f32) -> Option<usize> {
    if count == 0 || width <= 0.0 {
        return None;
    }
    if count == 1 {
        return Some(0);
    }
    let gap = width / (count - 1) as f32;
    let at = (x / gap).round();
    if at < 0.0 {
        return Some(0);
    }
    Some((at as usize).min(count - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticks land on ROUND numbers, which is the whole point of them.
    #[test]
    fn an_axis_is_marked_like_a_ruler() {
        // 0..100 in about five steps: every twenty.
        let t = axis_ticks(0.0, 100.0, 5);
        assert_eq!(t, vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0], "{t:?}");
        // A range that does not start at a round number still has round ticks.
        let t = axis_ticks(3.0, 97.0, 4);
        assert!(t.iter().all(|v| (v / 20.0).fract().abs() < 1e-9), "{t:?}");
        // Small numbers get small steps rather than a single tick.
        let t = axis_ticks(0.0, 1.0, 4);
        assert!(t.len() >= 4, "{t:?}");
        assert_eq!(t[0], 0.0);
        // Nothing nonsensical from nonsense.
        assert!(axis_ticks(f64::NAN, 1.0, 4).is_empty());
        assert!(axis_ticks(0.0, 1.0, 0).is_empty());
        // A FLAT series says its one value rather than drawing an empty axis.
        assert_eq!(axis_ticks(7.0, 7.0, 5), vec![7.0]);
        // Backwards is the same axis.
        assert_eq!(axis_ticks(100.0, 0.0, 5), axis_ticks(0.0, 100.0, 5));
        // And it TERMINATES on a range that invites a rounding loop.
        assert!(axis_ticks(0.1, 0.30000000000000004, 3).len() <= 5);
    }

    /// A label is read at a glance, so it is short and has no trailing zero.
    #[test]
    fn a_tick_says_the_number_in_as_few_characters_as_it_can() {
        assert_eq!(tick_label(0.0), "0");
        assert_eq!(tick_label(25.0), "25");
        assert_eq!(tick_label(0.5), "0.5");
        assert_eq!(tick_label(1200.0), "1.2k");
        assert_eq!(tick_label(1_200_000.0), "1.2M");
        assert_eq!(tick_label(3_000_000_000.0), "3G");
        assert_eq!(tick_label(-1500.0), "-1.5k");
        // Minus zero is zero.
        assert_eq!(tick_label(-0.0), "0");
    }

    /// Sampling keeps the LAST point, or the line stops short of the edge.
    #[test]
    fn sampling_never_drops_the_last_point() {
        assert_eq!(sample(0, 10), Vec::<usize>::new());
        assert_eq!(sample(3, 10), vec![0, 1, 2], "nothing to sample");
        let s = sample(1000, 100);
        assert!(s.len() <= 101, "{} points", s.len());
        assert_eq!(s[0], 0);
        assert_eq!(*s.last().unwrap(), 999, "the last row is always drawn");
        // Strictly increasing, so the line goes forwards.
        assert!(s.windows(2).all(|w| w[0] < w[1]));
        // A cap of nothing is not a chart of nothing.
        assert_eq!(sample(5, 0), vec![0, 1, 2, 3, 4]);
    }

    /// The slices of a pie add up to the whole pie.
    #[test]
    fn a_pie_is_a_whole_divided_up() {
        let s = slices(&[1.0, 1.0, 2.0]);
        assert_eq!(s.len(), 3);
        assert!((s[0].1 - 0.25).abs() < 1e-6, "{s:?}");
        assert!((s[2].1 - 0.5).abs() < 1e-6, "{s:?}");
        // Each starts where the last ended, and the last one ends at the top.
        assert_eq!(s[0].0, 0.0);
        assert!((s[1].0 - 0.25).abs() < 1e-6);
        assert!((s[2].0 + s[2].1 - 1.0).abs() < 1e-6);
        // A NEGATIVE share is not a share: it takes no arc and steals none.
        let s = slices(&[3.0, -1.0, 1.0]);
        assert_eq!(s[1].1, 0.0);
        assert!((s[0].1 + s[2].1 - 1.0).abs() < 1e-6, "{s:?}");
        // Nothing to divide is nothing drawn, not a circle of zeroes.
        assert!(slices(&[]).is_empty());
        assert!(slices(&[0.0, 0.0]).is_empty());
        assert!(slices(&[f64::NAN]).is_empty());
    }

    /// The nearest point is found by x, and never off the end.
    #[test]
    fn the_pointer_finds_a_point_and_stays_inside_the_series() {
        assert_eq!(nearest(0.0, 5, 100.0), Some(0));
        assert_eq!(nearest(100.0, 5, 100.0), Some(4));
        assert_eq!(nearest(51.0, 5, 100.0), Some(2));
        // Outside the canvas clamps rather than returning nothing: the pointer
        // leaving by a pixel should not make the tooltip blink.
        assert_eq!(nearest(-20.0, 5, 100.0), Some(0));
        assert_eq!(nearest(999.0, 5, 100.0), Some(4));
        // One point is always that point.
        assert_eq!(nearest(42.0, 1, 100.0), Some(0));
        // No points, no answer.
        assert_eq!(nearest(10.0, 0, 100.0), None);
        assert_eq!(nearest(10.0, 5, 0.0), None);
    }

    /// A kind survives the round trip through the store, and an unknown one
    /// opens rather than failing.
    #[test]
    fn a_kind_is_stored_as_a_word() {
        for k in Kind::all() {
            assert_eq!(Kind::from_key(k.key()), k, "{}", k.key());
        }
        assert_eq!(Kind::from_key("PIE"), Kind::Pie, "case is not meaning");
        assert_eq!(Kind::from_key(" line "), Kind::Line);
        // A kind from a newer client: drawn the plain way rather than refused.
        assert_eq!(Kind::from_key("sunburst"), Kind::Bars);
        assert_eq!(Kind::from_key(""), Kind::Bars);
        // Five, and all five distinct — a kind that mapped onto another's word
        // would overwrite it in the store.
        let keys: std::collections::BTreeSet<&str> = Kind::all().iter().map(|k| k.key()).collect();
        assert_eq!(keys.len(), 5, "{keys:?}");
    }
}
