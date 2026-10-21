//! Drawing on the remote screen: fading pen strokes and a laser pointer. The same model runs on the
//! viewer (drawn instantly over the video) and on the host (drawn in an overlay the remote user sees).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Pen strokes fade out over this time once the pen is lifted.
pub const STROKE_FADE: Duration = Duration::from_millis(1000);
/// The laser dot disappears this long after it last moved; its trail is shorter.
const LASER_HOLD: Duration = Duration::from_millis(1200);
const LASER_TRAIL: Duration = Duration::from_millis(350);
/// Line width as a fraction of the screen height (about 5 px at 1080p).
pub const STROKE_WIDTH: f32 = 0.0045;

pub const COLORS: [[u8; 3]; 5] = [[255, 59, 48], [255, 204, 0], [52, 199, 89], [10, 132, 255], [255, 255, 255]];

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum DrawEvent {
    /// Adds a point to a stroke (starting it if new). Coordinates are 0..=65535 over the screen.
    Point {
        stroke: u32,
        x: u16,
        y: u16,
        color: [u8; 3],
    },
    /// Pen lifted: the stroke starts fading.
    End {
        stroke: u32,
    },
    Laser {
        x: u16,
        y: u16,
    },
    Clear,
}

pub struct Stroke {
    pub id: u32,
    pub color: [u8; 3],
    /// Normalized 0..1 positions.
    pub points: Vec<(f32, f32)>,
    ended: Option<Instant>,
}

#[derive(Default)]
pub struct Ink {
    pub strokes: Vec<Stroke>,
    /// Laser positions with the time they were reached, newest last.
    laser: Vec<((f32, f32), Instant)>,
}

fn norm(v: u16) -> f32 {
    v as f32 / 65535.0
}

impl Ink {
    pub fn apply(&mut self, ev: &DrawEvent, now: Instant) {
        match *ev {
            DrawEvent::Point { stroke, x, y, color } => {
                let p = (norm(x), norm(y));
                match self.strokes.iter_mut().find(|s| s.id == stroke && s.ended.is_none()) {
                    Some(s) => s.points.push(p),
                    None => self.strokes.push(Stroke { id: stroke, color, points: vec![p], ended: None }),
                }
            }
            DrawEvent::End { stroke } => {
                for s in self.strokes.iter_mut().filter(|s| s.id == stroke && s.ended.is_none()) {
                    s.ended = Some(now);
                }
            }
            DrawEvent::Laser { x, y } => self.laser.push(((norm(x), norm(y)), now)),
            DrawEvent::Clear => {
                self.strokes.clear();
                self.laser.clear();
            }
        }
    }

    /// Opacity of a stroke now: 1 while drawing, then fading to 0.
    pub fn stroke_alpha(s: &Stroke, now: Instant) -> f32 {
        match s.ended {
            None => 1.0,
            Some(t) => 1.0 - now.duration_since(t).as_secs_f32() / STROKE_FADE.as_secs_f32(),
        }
        .clamp(0.0, 1.0)
    }

    /// The laser dot (position, opacity) if visible.
    pub fn laser_dot(&self, now: Instant) -> Option<((f32, f32), f32)> {
        let &(p, t) = self.laser.last()?;
        let age = now.duration_since(t).as_secs_f32();
        let hold = LASER_HOLD.as_secs_f32();
        (age < hold).then(|| (p, (1.0 - (age - hold * 0.6).max(0.0) / (hold * 0.4)).clamp(0.0, 1.0)))
    }

    /// Laser trail segments with their opacity, oldest first.
    pub fn laser_trail(&self, now: Instant) -> impl Iterator<Item = ((f32, f32), (f32, f32), f32)> + '_ {
        self.laser.windows(2).filter_map(move |w| {
            let age = now.duration_since(w[1].1).as_secs_f32() / LASER_TRAIL.as_secs_f32();
            (age < 1.0).then(|| (w[0].0, w[1].0, 1.0 - age))
        })
    }

    /// Drops everything fully faded. Returns true while something is still visible.
    pub fn prune(&mut self, now: Instant) -> bool {
        self.strokes.retain(|s| Self::stroke_alpha(s, now) > 0.0);
        // Keep only what the dot or the trail still needs.
        let keep_from =
            self.laser.iter().position(|(_, t)| now.duration_since(*t) < LASER_TRAIL).unwrap_or(self.laser.len());
        let keep_from = keep_from.saturating_sub(1).min(self.laser.len().saturating_sub(1));
        if self.laser_dot(now).is_none() {
            self.laser.clear();
        } else {
            self.laser.drain(..keep_from);
        }
        !self.strokes.is_empty() || !self.laser.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strokes_fade_after_pen_up() {
        let t0 = Instant::now();
        let mut ink = Ink::default();
        ink.apply(&DrawEvent::Point { stroke: 1, x: 0, y: 0, color: COLORS[0] }, t0);
        ink.apply(&DrawEvent::Point { stroke: 1, x: 65535, y: 65535, color: COLORS[0] }, t0);
        assert_eq!(ink.strokes[0].points.len(), 2);
        assert_eq!(Ink::stroke_alpha(&ink.strokes[0], t0 + Duration::from_secs(5)), 1.0, "no fade while drawing");
        ink.apply(&DrawEvent::End { stroke: 1 }, t0);
        let half = Ink::stroke_alpha(&ink.strokes[0], t0 + STROKE_FADE / 2);
        assert!((0.4..0.6).contains(&half), "{half}");
        assert!(ink.prune(t0 + STROKE_FADE / 2));
        assert!(!ink.prune(t0 + STROKE_FADE + Duration::from_millis(1)));
    }

    #[test]
    fn laser_disappears() {
        let t0 = Instant::now();
        let mut ink = Ink::default();
        ink.apply(&DrawEvent::Laser { x: 100, y: 100 }, t0);
        assert!(ink.laser_dot(t0).is_some());
        assert!(ink.prune(t0 + Duration::from_millis(500)));
        assert!(!ink.prune(t0 + LASER_HOLD + Duration::from_millis(1)));
    }
}
