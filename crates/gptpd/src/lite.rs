//! An end station's AVB Lite side: whether to fall back, decided from the
//! Pdelay exchange (gptp::lite), and the profile's end-to-end client
//! (gptp::e2e), which runs in place of gPTP while the fallback holds.

use std::time::Duration;

use gptp::e2e::{self, Client};
use gptp::lite::{self, Fallback, Reason};
use gptp::{ClockIdentity, MessageType, PortIdentity};

use crate::config::Profile;
use crate::station::View;

/// A frame to send: where to, its octets after the Ethernet header, and
/// whether its transmit time is wanted.
pub struct Outgoing {
    pub destination: [u8; 6],
    pub frame: Vec<u8>,
    pub stamp: Option<(MessageType, u16)>,
}

pub struct Lite {
    /// None where the station runs gPTP only.
    fallback: Option<Fallback>,
    pub client: Client,
    active: bool,
    /// The port's link speed, Mb/s, for the asymmetry.
    link_speed: Option<u32>,
}

impl Lite {
    pub fn new(identity: ClockIdentity, profile: Profile, now: Duration) -> Self {
        let fallback = match profile {
            Profile::Gptp => None,
            Profile::Auto => Some(Fallback::new(identity, false, now)),
            Profile::AvbLite => Some(Fallback::new(identity, true, now)),
        };
        Self {
            fallback,
            client: Client::new(e2e::Config::avb_lite(PortIdentity {
                clock: identity,
                port: 1,
            })),
            active: false,
            link_speed: None,
        }
    }

    /// Whether the port runs the AVB Lite profile now.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Why it fell back, while it holds.
    pub fn reason(&self) -> Option<Reason> {
        self.fallback.as_ref().and_then(Fallback::reason)
    }

    /// A PTP frame heard, gPTP's or the profile's, and when it came on the
    /// port's clock.
    pub fn handle_frame(&mut self, now: Duration, frame: &[u8], received: i128) {
        if let Some(fallback) = &mut self.fallback {
            fallback.handle_message(now, frame);
        }
        self.client.handle_frame(now, frame, received);
    }

    /// The gPTP link sent the Pdelay_Req numbered `sequence`.
    pub fn handle_request(&mut self, now: Duration, sequence: u16) {
        if let Some(fallback) = &mut self.fallback {
            fallback.handle_request(now, sequence);
        }
    }

    /// The link came up, at `speed` Mb/s, or went down.
    pub fn set_link(&mut self, now: Duration, up: bool, speed: Option<u32>) {
        self.link_speed = speed.filter(|_| up);
        if let Some(fallback) = &mut self.fallback {
            fallback.set_link(now, up);
        }
        if self.active {
            self.client.set_link(now, up);
        }
    }

    pub fn poll_timeout(&self) -> Option<Duration> {
        let fallback = self.fallback.as_ref().and_then(Fallback::poll_timeout);
        [fallback, self.client.poll_timeout()]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn handle_timeout(&mut self, now: Duration) {
        if let Some(fallback) = &mut self.fallback {
            fallback.handle_timeout(now);
        }
        self.client.handle_timeout(now);
    }

    /// Changes profile as the fallback decided: true on falling back to
    /// AVB Lite, false on returning to gPTP, none while it stands.
    pub fn decide(&mut self, now: Duration, link_up: bool) -> Option<bool> {
        let fall_back = self.reason().is_some();
        if fall_back == self.active {
            return None;
        }
        self.active = fall_back;
        self.client.set_link(now, fall_back && link_up);
        Some(fall_back)
    }

    pub fn poll_transmit(&mut self) -> Option<Outgoing> {
        if let Some(beacon) = self.fallback.as_mut().and_then(Fallback::poll_transmit) {
            return Some(Outgoing {
                destination: gptp::DESTINATION,
                frame: beacon,
                stamp: None,
            });
        }
        self.client.poll_transmit().map(|transmit| Outgoing {
            destination: e2e::DESTINATION,
            frame: transmit.frame,
            stamp: transmit.stamp,
        })
    }

    /// The grandmaster's time as the client has it, the asymmetry from
    /// the grandmaster's link speed applied once known.
    pub fn view(&mut self) -> View {
        let speeds = self.client.grandmaster_link_speed().zip(self.link_speed);
        if let Some((grandmaster, local)) = speeds {
            self.client
                .set_asymmetry(lite::link_asymmetry(grandmaster, local));
        }
        View {
            grandmaster: self
                .client
                .grandmaster()
                .map(|grandmaster| grandmaster.clock),
            ours: false,
            line: self.client.time_line(),
            asymmetry_included: speeds.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ClockIdentity {
        ClockIdentity(0xa010_a3ff_fea5_1726)
    }

    fn second(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn auto_falls_back_when_nothing_answers_and_returns_as_the_link_comes_up() {
        let mut lite = Lite::new(identity(), Profile::Auto, second(0));
        lite.set_link(second(0), true, Some(1000));
        assert_eq!(lite.decide(second(0), true), None);
        for sequence in 0..12 {
            lite.handle_request(second(u64::from(sequence) + 1), sequence);
        }
        assert_eq!(lite.reason(), Some(Reason::PdelayUnanswered));
        assert_eq!(lite.decide(second(13), true), Some(true));
        assert!(lite.active());
        // The beacon goes out in place of the link's own requests.
        assert!(lite.poll_timeout().is_some_and(|due| due <= second(13)));
        lite.handle_timeout(second(13));
        let beacon = lite.poll_transmit().expect("beacon");
        assert_eq!(beacon.destination, gptp::DESTINATION);
        assert!(lite::has_endpoint_tlv(&beacon.frame, 54));
        lite.set_link(second(20), false, None);
        lite.set_link(second(21), true, Some(1000));
        assert_eq!(lite.decide(second(21), true), Some(false));
        assert!(!lite.active());
    }

    #[test]
    fn avb_lite_configured_falls_back_at_once_and_holds() {
        let mut lite = Lite::new(identity(), Profile::AvbLite, second(0));
        assert_eq!(lite.reason(), Some(Reason::Configured));
        assert_eq!(lite.decide(second(0), true), Some(true));
        lite.set_link(second(5), false, None);
        lite.set_link(second(6), true, Some(100));
        assert_eq!(lite.decide(second(6), true), None);
        assert!(lite.active());
    }

    #[test]
    fn gptp_only_never_falls_back() {
        let mut lite = Lite::new(identity(), Profile::Gptp, second(0));
        for sequence in 0..20 {
            lite.handle_request(second(u64::from(sequence)), sequence);
        }
        assert_eq!(lite.reason(), None);
        assert_eq!(lite.decide(second(30), true), None);
        assert!(lite.poll_transmit().is_none());
    }
}
