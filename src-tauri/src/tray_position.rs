#[derive(Debug, PartialEq)]
pub struct PopupRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn place(
    anchor: (f64, f64),
    origin: (i32, i32),
    work: (u32, u32),
    scale: f64,
) -> Option<PopupRect> {
    if work.0 == 0
        || work.1 == 0
        || !scale.is_finite()
        || scale <= 0.0
        || !anchor.0.is_finite()
        || !anchor.1.is_finite()
    {
        return None;
    }
    let width = (380.0 * scale).round().clamp(1.0, work.0 as f64);
    let height = (410.0 * scale).round().clamp(1.0, work.1 as f64);
    let left = origin.0 as f64;
    let top = origin.1 as f64;
    let right = left + work.0 as f64;
    let bottom = top + work.1 as f64;
    let gap = 4.0 * scale;
    let below = anchor.1 + gap;
    let y = if below + height <= bottom {
        below
    } else {
        anchor.1 - height - gap
    };
    Some(PopupRect {
        x: (anchor.0 - width / 2.0).clamp(left, right - width).round() as i32,
        y: y.clamp(top, bottom - height).round() as i32,
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bottom_taskbar_popup_stays_above_work_area_edge() {
        let r = place((1900.0, 1060.0), (0, 0), (1920, 1040), 1.0).unwrap();
        assert_eq!(
            r,
            PopupRect {
                x: 1540,
                y: 630,
                width: 380,
                height: 410
            }
        );
    }
    #[test]
    fn top_menu_bar_popup_stays_below_menu() {
        let r = place((800.0, 12.0), (0, 24), (1440, 876), 1.0).unwrap();
        assert_eq!(r.y, 24);
    }
    #[test]
    fn scaled_negative_origin_monitor_is_not_clamped_to_primary() {
        let r = place((-20.0, 1400.0), (-2560, -100), (2560, 1480), 1.5).unwrap();
        assert_eq!(r.width, 570);
        assert_eq!(r.height, 615);
        assert_eq!(r.x, -570);
        assert!(r.y >= -100 && r.y + r.height as i32 <= 1380);
    }
    #[test]
    fn all_taskbar_edges_and_small_monitors_remain_inside_work_area() {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            for (origin, work) in [
                ((0, 0), (1920, 1040)),
                ((40, 0), (1880, 1080)),
                ((-1600, 40), (1600, 860)),
                ((100, 100), (300, 240)),
            ] {
                for anchor in [
                    (0.0, 500.0),
                    (1910.0, 500.0),
                    (1900.0, 1070.0),
                    (-1500.0, 0.0),
                ] {
                    let r = place(anchor, origin, work, scale).unwrap();
                    assert!(r.x >= origin.0 && r.y >= origin.1);
                    assert!(r.x as i64 + r.width as i64 <= origin.0 as i64 + work.0 as i64);
                    assert!(r.y as i64 + r.height as i64 <= origin.1 as i64 + work.1 as i64);
                }
            }
        }
    }
    #[test]
    fn invalid_monitor_metadata_fails_without_panicking() {
        assert!(place((0.0, 0.0), (0, 0), (0, 10), 1.0).is_none());
        assert!(place((0.0, 0.0), (0, 0), (10, 10), f64::NAN).is_none());
    }
}
