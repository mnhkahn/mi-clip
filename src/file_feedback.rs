//! File-only menu-bar feedback. Contains no filenames, paths or file contents.
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Unavailable,
    Disconnected,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
pub enum Event {
    Requested,
    Progress {
        received: u64,
        total: u64,
    },
    /// Emit only after all files have been verified and committed to disk.
    Saved {
        files: u32,
    },
    Failed(Failure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Idle,
    Connecting,
    Receiving,
    Saved,
    Failed,
}

enum State {
    Idle,
    Connecting,
    Receiving { received: u64, total: u64 },
    Saved { files: u32 },
    Failed(Failure),
}

pub struct Feedback {
    state: State,
    since: Duration,
    unread: bool,
    request_flash: bool,
}
impl Default for Feedback {
    fn default() -> Self {
        Self {
            state: State::Idle,
            since: Duration::ZERO,
            unread: false,
            request_flash: false,
        }
    }
}
impl Feedback {
    /// Ignore stale/invalid progress and premature completion events.
    pub fn apply(&mut self, event: Event, now: Duration) -> bool {
        match event {
            Event::Requested => {
                if self.active() {
                    return false;
                }
                self.state = State::Connecting;
                self.request_flash = false;
            }
            Event::Progress { received, total } => {
                if received > total {
                    return false;
                }
                match self.state {
                    State::Connecting => (),
                    State::Receiving {
                        received: previous,
                        total: expected,
                    } if total == expected && received >= previous => {
                        self.state = State::Receiving { received, total };
                        return true;
                    }
                    _ => return false,
                }
                self.state = State::Receiving { received, total };
            }
            Event::Saved { files } => {
                if files == 0
                    || !matches!(self.state, State::Receiving { received, total } if received == total)
                {
                    return false;
                }
                self.state = State::Saved { files };
            }
            Event::Failed(reason) => {
                if !self.active() {
                    return false;
                }
                // A request unsupported by this release still gets a short
                // attention animation, without pretending bytes were received.
                self.request_flash = reason == Failure::Unavailable;
                self.state = State::Failed(reason);
            }
        }
        self.since = now;
        self.unread = true;
        true
    }
    pub fn active(&self) -> bool {
        matches!(self.state, State::Connecting | State::Receiving { .. })
    }
    pub fn acknowledge(&mut self) {
        if !self.active() {
            self.unread = false;
        }
    }
    pub fn tick(&mut self, now: Duration) -> bool {
        if matches!(self.state, State::Connecting)
            && now.saturating_sub(self.since) >= Duration::from_secs(20)
        {
            return self.apply(Event::Failed(Failure::TimedOut), now);
        }
        false
    }
    pub fn appearance(&self, now: Duration) -> (Icon, usize, bool) {
        let elapsed = now.saturating_sub(self.since);
        let frame = (elapsed.as_millis() / 83 % 12) as usize;
        match self.state {
            State::Connecting => (Icon::Connecting, frame, true),
            State::Receiving { .. } => (Icon::Receiving, frame, true),
            State::Saved { .. } if self.unread => (
                Icon::Saved,
                if elapsed < Duration::from_secs(2) {
                    frame
                } else {
                    0
                },
                elapsed < Duration::from_secs(2),
            ),
            State::Failed(_) if self.unread => (
                Icon::Failed,
                if self.request_flash && elapsed < Duration::from_millis(1200) {
                    frame
                } else {
                    0
                },
                self.request_flash && elapsed < Duration::from_millis(1200),
            ),
            _ => (Icon::Idle, 0, false),
        }
    }
    pub fn detail(&self) -> Option<String> {
        Some(match self.state {
            State::Idle => return None,
            State::Connecting => "收到分享请求 · 正在建立文件连接".into(),
            State::Receiving { received, total } => format!(
                "正在接收文件 · {}%",
                if total == 0 {
                    100
                } else {
                    u128::from(received) * 100 / u128::from(total)
                }
            ),
            State::Saved { files } => format!("已接收并保存 {files} 个文件"),
            State::Failed(Failure::Unavailable) => "收到分享请求 · 此版本尚不支持文件接收".into(),
            State::Failed(Failure::Disconnected) => "文件连接已断开 · 未完成接收".into(),
            State::Failed(Failure::TimedOut) => "文件连接超时 · 未收到文件".into(),
        })
    }
}

/// Keep simultaneous transfers separate; a failed/disconnected peer must not
/// overwrite another peer's active transfer or discard its progress.
pub struct FeedbackGroup {
    peers: std::collections::HashMap<Option<std::net::SocketAddr>, Feedback>,
    latest: Option<std::net::SocketAddr>,
}
impl Default for FeedbackGroup {
    fn default() -> Self {
        Self {
            peers: [(None, Feedback::default())].into(),
            latest: None,
        }
    }
}
impl FeedbackGroup {
    fn visible(&self) -> &Feedback {
        self.peers
            .values()
            .filter(|feedback| feedback.active())
            .max_by_key(|feedback| feedback.since)
            .unwrap_or_else(|| &self.peers[&self.latest])
    }
    pub fn apply_for(
        &mut self,
        peer: Option<std::net::SocketAddr>,
        event: Event,
        now: Duration,
    ) -> bool {
        let changed = self.peers.entry(peer).or_default().apply(event, now);
        if changed {
            self.latest = peer;
        }
        changed
    }
    pub fn active(&self) -> bool {
        self.peers.values().any(Feedback::active)
    }
    pub fn acknowledge(&mut self) {
        for feedback in self.peers.values_mut() {
            feedback.acknowledge();
        }
    }
    pub fn tick(&mut self, now: Duration) -> bool {
        let mut changed = false;
        for feedback in self.peers.values_mut() {
            changed |= feedback.tick(now);
        }
        changed
    }
    pub fn appearance(&self, now: Duration) -> (Icon, usize, bool) {
        self.visible().appearance(now)
    }
    pub fn detail(&self) -> Option<String> {
        let active = self
            .peers
            .values()
            .filter(|feedback| feedback.active())
            .count();
        if active > 1 {
            Some(format!("正在接收文件 · {active} 台设备"))
        } else {
            self.visible().detail()
        }
    }
}

/// Tiny vector frames, decoded lazily by AppKit. No bitmap assets or UI runtime.
pub fn icon_pdf(icon: Icon, frame: usize) -> Vec<u8> {
    use std::fmt::Write;
    let color = match icon {
        Icon::Connecting | Icon::Failed => "0.90 0.47 0.06",
        Icon::Receiving => "0.10 0.48 0.94",
        Icon::Saved => "0.12 0.65 0.36",
        Icon::Idle => "0.5 0.5 0.5",
    };
    let mut stream = format!(
        "q\n1 0 0 -1 0 24 cm\n{color} RG\n1.7 w\n1 J\n1 j\n6 4 m 4 4 l 4 21 l 20 21 l 20 4 l 18 4 l S\n8 3 m 16 3 l 16 7 l 8 7 l h S\n7 11 m 17 11 l 14 8 l S\n17 16 m 7 16 l 10 19 l S\n{color} rg\n"
    );
    let circle = |out: &mut String, x: f64, y: f64, r: f64| {
        let k = r * 0.55228475;
        let _ = writeln!(
            out,
            "{} {y} m {} {} {} {} {x} {} c {} {} {} {} {} {y} c {} {} {} {} {x} {} c {} {} {} {} {} {y} c f",
            x + r,
            x + r,
            y + k,
            x + k,
            y + r,
            y + r,
            x - k,
            y + r,
            x - r,
            y + k,
            x - r,
            x - r,
            y - k,
            x - k,
            y - r,
            y - r,
            x + k,
            y - r,
            x + r,
            y - k,
            x + r
        );
    };
    circle(&mut stream, 19., 19., 5.);
    stream.push_str("1 1 1 RG\n1 1 1 rg\n1.5 w\n");
    match icon {
        Icon::Saved => {
            // Two soft expanding rings, then a steady unread check badge.
            let r = 2. + (frame % 6) as f64 * 0.5;
            if frame != 0 {
                let _ = writeln!(
                    stream,
                    "{r} 0 0 {r} 19 19 cm\n0.13 w\n1 0 m 1 0.5523 0.5523 1 0 1 c -0.5523 1 -1 0.5523 -1 0 c -1 -0.5523 -0.5523 -1 0 -1 c 0.5523 -1 1 -0.5523 1 0 c S\n{} 0 0 {} {} {} cm",
                    1. / r,
                    1. / r,
                    -19. / r,
                    -19. / r
                );
            }
            stream.push_str("1.5 w\n16.4 19 m 18.2 20.8 l 21.8 17.3 l S\n");
        }
        Icon::Receiving => {
            let angle = frame as f64 * std::f64::consts::TAU / 12.;
            circle(
                &mut stream,
                19. + angle.cos() * 2.7,
                19. + angle.sin() * 2.7,
                1.1,
            );
        }
        Icon::Connecting => {
            for x in [16.5, 19., 21.5] {
                circle(&mut stream, x, 19., 0.7);
            }
        }
        Icon::Failed => stream.push_str("19 16.2 m 19 19.4 l S\n19 21 m 19 21.1 l S\n"),
        Icon::Idle => (),
    }
    stream.push_str("Q\n");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 24 24] /Resources << >> /Contents 4 0 R >>"
            .to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
    ];
    let mut pdf = "%PDF-1.4\n".to_owned();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        let _ = writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1);
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        let _ = writeln!(pdf, "{offset:010} 00000 n ");
    }
    let _ = writeln!(
        pdf,
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    );
    pdf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_completed_transfer_can_turn_green_and_badge_waits_for_acknowledgement() {
        let mut feedback = Feedback::default();
        let now = Duration::ZERO;
        assert!(!feedback.apply(Event::Saved { files: 1 }, now));
        feedback.apply(Event::Requested, now);
        assert!(!feedback.apply(Event::Saved { files: 1 }, now));
        feedback.apply(
            Event::Progress {
                received: 4,
                total: 10,
            },
            now,
        );
        assert!(!feedback.apply(
            Event::Progress {
                received: 3,
                total: 10
            },
            now
        ));
        assert!(!feedback.apply(
            Event::Progress {
                received: 5,
                total: 11
            },
            now
        ));
        assert!(!feedback.apply(Event::Saved { files: 1 }, now));
        feedback.apply(
            Event::Progress {
                received: 10,
                total: 10,
            },
            now,
        );
        assert!(feedback.apply(Event::Saved { files: 1 }, now));
        assert_eq!(feedback.appearance(Duration::from_secs(1)).0, Icon::Saved);
        assert_eq!(
            feedback.appearance(Duration::from_secs(3)),
            (Icon::Saved, 0, false)
        );
        assert!(!feedback.apply(Event::Failed(Failure::Disconnected), now));
        feedback.acknowledge();
        assert_eq!(feedback.appearance(now), (Icon::Idle, 0, false));
    }
    #[test]
    fn unavailable_request_flashes_then_keeps_warning_without_idle_timer() {
        let mut feedback = Feedback::default();
        feedback.apply(Event::Requested, Duration::ZERO);
        feedback.apply(Event::Failed(Failure::Unavailable), Duration::ZERO);
        assert!(feedback.appearance(Duration::from_millis(500)).2);
        assert_eq!(
            feedback.appearance(Duration::from_secs(2)),
            (Icon::Failed, 0, false)
        );
        assert!(!feedback.apply(Event::Saved { files: 1 }, Duration::ZERO));
        feedback.acknowledge();
        assert_eq!(
            feedback.appearance(Duration::from_secs(2)),
            (Icon::Idle, 0, false)
        );
    }
    #[test]
    fn timed_out_request_stops_animation_and_retry_starts_fresh() {
        let mut feedback = Feedback::default();
        feedback.apply(Event::Requested, Duration::ZERO);
        feedback.acknowledge();
        assert!(feedback.active());
        feedback.tick(Duration::from_secs(20));
        assert_eq!(
            feedback.appearance(Duration::from_secs(20)),
            (Icon::Failed, 0, false)
        );
        assert!(feedback.apply(Event::Requested, Duration::from_secs(21)));
        assert!(feedback.active());
    }
}

#[cfg(test)]
mod multi_tests {
    use super::*;
    #[test]
    fn disconnect_and_completion_do_not_interrupt_other_peers() {
        let first = Some("192.0.2.1:1".parse().unwrap());
        let second = Some("192.0.2.2:1".parse().unwrap());
        let mut feedback = FeedbackGroup::default();
        for peer in [first, second] {
            assert!(feedback.apply_for(peer, Event::Requested, Duration::ZERO));
            assert!(feedback.apply_for(
                peer,
                Event::Progress {
                    received: 1,
                    total: 10
                },
                Duration::ZERO
            ));
        }
        assert!(feedback.detail().unwrap().contains("2 台设备"));
        feedback.apply_for(
            first,
            Event::Failed(Failure::Disconnected),
            Duration::from_secs(1),
        );
        assert!(feedback.active());
        assert_eq!(
            feedback.appearance(Duration::from_secs(1)).0,
            Icon::Receiving
        );
        assert!(feedback.apply_for(
            second,
            Event::Progress {
                received: 10,
                total: 10
            },
            Duration::from_secs(2)
        ));
        assert!(feedback.apply_for(second, Event::Saved { files: 1 }, Duration::from_secs(2)));
        assert!(!feedback.active());
        assert_eq!(feedback.appearance(Duration::from_secs(2)).0, Icon::Saved);
    }
}
