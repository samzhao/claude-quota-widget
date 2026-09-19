//! The menubar item: which account has the most room left, at a glance.

use tauri::image::Image;

const ICON_PX: u32 = 44; // 22pt at 2x
const WARNING_AT: f64 = 70.0;
const CRITICAL_AT: f64 = 90.0;
const MAX_NAME_CHARS: usize = 16;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Level {
    Normal,
    Warning,
    Critical,
}

/// One account reduced to what the menubar needs.
pub struct Candidate<'a> {
    pub label: &'a str,
    /// Percent used per limit. Empty = no reading, so it cannot be recommended.
    pub utilizations: Vec<f64>,
}

#[derive(Debug, PartialEq)]
pub struct Summary {
    pub title: String,
    pub tooltip: String,
    pub level: Level,
    /// Tightest limit of the recommended account, 0-100.
    pub percent: f64,
}

fn short_name(label: &str) -> String {
    let local = label.split('@').next().unwrap_or(label);
    if local.chars().count() <= MAX_NAME_CHARS {
        local.to_string()
    } else {
        let cut: String = local.chars().take(MAX_NAME_CHARS - 1).collect();
        format!("{cut}…")
    }
}

/// An account is only as usable as its tightest limit, so rank accounts by
/// their highest utilization and recommend the lowest.
pub fn summarize(candidates: &[Candidate]) -> Summary {
    let best = candidates
        .iter()
        .filter(|c| !c.utilizations.is_empty())
        .map(|c| (c, c.utilizations.iter().cloned().fold(0.0_f64, f64::max)))
        .min_by(|a, b| a.1.total_cmp(&b.1));

    let Some((account, tightest)) = best else {
        return Summary {
            title: "–".to_string(),
            tooltip: "Claude Quota: no readings yet".to_string(),
            level: Level::Normal,
            percent: 0.0,
        };
    };
    let level = if tightest >= CRITICAL_AT {
        Level::Critical
    } else if tightest >= WARNING_AT {
        Level::Warning
    } else {
        Level::Normal
    };
    Summary {
        title: format!("{} {}%", short_name(account.label), tightest.round()),
        tooltip: format!(
            "Most room left: {} (tightest limit at {}%)",
            account.label,
            tightest.round()
        ),
        level,
        percent: tightest,
    }
}

fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A ring gauge drawn by hand: faint full ring, solid arc clockwise from
/// 12 o'clock for the used share. Normal is black for a template image (macOS
/// recolors it for light and dark menubars); warning and critical carry color.
pub fn gauge_icon(percent: f64, level: Level) -> Image<'static> {
    let (r, g, b) = match level {
        Level::Normal => (0u8, 0u8, 0u8),
        Level::Warning => (226, 167, 60),
        Level::Critical => (238, 118, 96),
    };
    let size = ICON_PX as f64;
    let center = size / 2.0;
    let (outer, inner) = (size * 0.40, size * 0.25);
    let used = (percent / 100.0).clamp(0.0, 1.0) * std::f64::consts::TAU;

    let mut rgba = Vec::with_capacity((ICON_PX * ICON_PX * 4) as usize);
    for y in 0..ICON_PX {
        for x in 0..ICON_PX {
            let (dx, dy) = (x as f64 + 0.5 - center, y as f64 + 0.5 - center);
            let distance = (dx * dx + dy * dy).sqrt();
            // 1px soft edges on both sides of the ring.
            let ring = smoothstep(inner - 0.5, inner + 0.5, distance)
                * (1.0 - smoothstep(outer - 0.5, outer + 0.5, distance));
            // Angle measured clockwise from 12 o'clock.
            let angle = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU);
            let strength = if angle <= used { 1.0 } else { 0.28 };
            rgba.extend_from_slice(&[r, g, b, (ring * strength * 255.0).round() as u8]);
        }
    }
    Image::new_owned(rgba, ICON_PX, ICON_PX)
}

pub fn is_template(level: Level) -> bool {
    level == Level::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate<'a>(label: &'a str, utilizations: &[f64]) -> Candidate<'a> {
        Candidate { label, utilizations: utilizations.to_vec() }
    }

    #[test]
    fn recommends_the_account_whose_tightest_limit_is_lowest() {
        let summary = summarize(&[
            candidate("you@example.com", &[6.0, 79.0, 94.0]),
            candidate("dev@example.com", &[0.0, 63.0, 33.0]),
            candidate("work@example.com", &[52.0, 66.0, 64.0]),
        ]);
        assert_eq!(summary.title, "dev 63%");
        assert_eq!(summary.level, Level::Normal);
    }

    #[test]
    fn level_reflects_the_best_account_not_the_worst() {
        let summary = summarize(&[
            candidate("a@example.com", &[10.0, 95.0, 40.0]),
            candidate("b@example.com", &[10.0, 72.0, 40.0]),
        ]);
        assert_eq!(summary.title, "b 72%");
        assert_eq!(summary.level, Level::Warning);
    }

    #[test]
    fn accounts_without_readings_are_never_recommended() {
        let summary = summarize(&[
            candidate("dead@example.com", &[]),
            candidate("ok@example.com", &[91.0]),
        ]);
        assert_eq!(summary.title, "ok 91%");
        assert_eq!(summary.level, Level::Critical);
        assert_eq!(summarize(&[candidate("dead@example.com", &[])]).title, "–");
    }

    #[test]
    fn long_names_are_cut_so_the_menubar_stays_narrow() {
        let summary = summarize(&[candidate("averyveryverylongname@example.com", &[5.0])]);
        assert_eq!(summary.title, "averyveryverylo… 5%");
    }

    #[test]
    fn gauge_icon_has_the_right_shape() {
        let icon = gauge_icon(50.0, Level::Warning);
        assert_eq!(icon.rgba().len(), (ICON_PX * ICON_PX * 4) as usize);
        let alpha_at = |x: u32, y: u32| icon.rgba()[((y * ICON_PX + x) * 4 + 3) as usize];
        assert_eq!(alpha_at(22, 22), 0, "hole in the middle");
        assert_eq!(alpha_at(0, 0), 0, "transparent corner");
        // Right side (3 o'clock, inside the used half) is solid; left side is faint.
        assert!(alpha_at(36, 22) > 200);
        assert!(alpha_at(7, 22) < 100 && alpha_at(7, 22) > 0);
    }
}
