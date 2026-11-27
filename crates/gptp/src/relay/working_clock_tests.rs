//! A working clock on domain 1 between systems wired back to back, each
//! port's role configured, beside domain 0's relays.

use super::*;
use std::vec::Vec;

/// The LocalClock's reading at the start, on the leading system.
const LEADER_BASE: i128 = 5_000_000_000;
/// The following system's LocalClock: from where, and at what rate over
/// the leader's.
const FOLLOWER_BASE: i128 = 91_000_000_000;
const FOLLOWER_RATE: f64 = 1.0 + 30e-6;
/// The cable's delay each way, and a frame's time from queued to sent.
const WIRE: i128 = 240;
const TRANSMIT_LATENCY: i128 = 15_000;

fn leader_identity() -> ClockIdentity {
    ClockIdentity(0x8c1f_64ff_fe36_c210)
}

fn follower_identity() -> ClockIdentity {
    ClockIdentity(0x8c1f_64ff_fe36_c220)
}

/// One system on the wire: its relays, by domain, and its LocalClock.
struct System {
    relays: Vec<Relay>,
    base: i128,
    rate: f64,
}

impl System {
    fn clock(&self, nanos: i128) -> i128 {
        self.base + (nanos as f64 * self.rate) as i128
    }

    fn stamp(&self, nanos: i128) -> Stamp {
        Stamp::local(self.clock(nanos))
    }
}

/// A frame one system sent, as the other heard it.
struct Heard {
    from_leader: bool,
    domain: u8,
    major_sdo_id: u8,
    message_type: MessageType,
    log_interval: i8,
    flags: u16,
}

/// Two systems, one port each, joined by a cable.
struct Wire {
    leader: System,
    follower: System,
    now: Duration,
    heard: Vec<Heard>,
}

impl Wire {
    /// Domain 0's relays, and with `working_clock` domain 1's too, the
    /// leader's port its timetransmitter and the follower's its
    /// timereceiver.
    fn new(working_clock: bool, roles: (Role, Role)) -> Self {
        let relays = |identity: ClockIdentity, role: Role| {
            let mut relays = std::vec![Relay::new(Config::new(identity), 1)];
            if working_clock {
                let mut relay = Relay::new(Config::working_clock(identity, 1), 1);
                relay.set_external_role(Duration::ZERO, 1, role);
                relays.push(relay);
            }
            relays
        };
        let mut wire = Self {
            leader: System {
                relays: relays(leader_identity(), roles.0),
                base: LEADER_BASE,
                rate: 1.0,
            },
            follower: System {
                relays: relays(follower_identity(), roles.1),
                base: FOLLOWER_BASE,
                rate: FOLLOWER_RATE,
            },
            now: Duration::ZERO,
            heard: Vec::new(),
        };
        for system in [&mut wire.leader, &mut wire.follower] {
            for relay in &mut system.relays {
                relay.set_port(Duration::ZERO, 1, true);
            }
        }
        wire.carry();
        wire
    }

    fn nanos(&self) -> i128 {
        self.now.as_nanos() as i128
    }

    /// Runs to `until`, from timer to timer.
    fn run(&mut self, until: Duration) {
        loop {
            let next = [&self.leader, &self.follower]
                .into_iter()
                .flat_map(|system| system.relays.iter())
                .filter_map(Relay::poll_timeout)
                .min();
            let Some(next) = next.filter(|next| *next <= until) else {
                self.now = until;
                return;
            };
            self.now = next.max(self.now);
            let now = self.now;
            for system in [&mut self.leader, &mut self.follower] {
                for relay in &mut system.relays {
                    if relay.poll_timeout().is_some_and(|due| due <= now) {
                        relay.handle_timeout(now);
                    }
                }
            }
            self.carry();
        }
    }

    /// Carries every frame queued to the other system, each to all its
    /// relays, reporting when event messages left, until none is left.
    fn carry(&mut self) {
        loop {
            let mut carried = false;
            for from_leader in [true, false] {
                for index in 0..2 {
                    loop {
                        let (sender, receiver) = if from_leader {
                            (&mut self.leader, &mut self.follower)
                        } else {
                            (&mut self.follower, &mut self.leader)
                        };
                        let Some(relay) = sender.relays.get_mut(index) else {
                            break;
                        };
                        let Some(transmit) = relay.poll_transmit() else {
                            break;
                        };
                        carried = true;
                        let left = self.now.as_nanos() as i128 + TRANSMIT_LATENCY;
                        if let Some((message_type, sequence)) = transmit.stamp {
                            let stamp = sender.stamp(left);
                            sender.relays[index].handle_transmit_time(
                                1,
                                message_type,
                                sequence,
                                stamp,
                            );
                        }
                        let message = Message::decode(&transmit.frame).expect("decodes");
                        self.heard.push(Heard {
                            from_leader,
                            domain: message.header.domain,
                            major_sdo_id: message.header.major_sdo_id,
                            message_type: message.message_type(),
                            log_interval: message.header.log_interval,
                            flags: message.header.flags.0,
                        });
                        let arrived = receiver.stamp(left + WIRE);
                        for relay in &mut receiver.relays {
                            relay.handle_frame(self.now, 1, &transmit.frame, arrived);
                        }
                    }
                }
            }
            if !carried {
                return;
            }
        }
    }

    /// How far the follower's domain 1 time stands from the leader's now.
    fn error(&self) -> Option<i128> {
        let nanos = self.nanos();
        let followed = self.follower.relays[1].grandmaster_time(self.follower.clock(nanos))?;
        let led = self.leader.relays[1].grandmaster_time(self.leader.clock(nanos))?;
        Some(followed - led)
    }
}

#[test]
fn a_working_clock_locks_within_50_ms_of_the_link_coming_up() {
    let mut wire = Wire::new(true, (Role::TimeTransmitter, Role::TimeReceiver));
    let mut locked = None;
    let mut at = Duration::ZERO;
    while at < Duration::from_millis(50) {
        at += Duration::from_millis(1);
        wire.run(at);
        if locked.is_none() && wire.error().is_some() {
            locked = Some(at);
        }
    }
    let locked = locked.expect("locked within 50 ms");
    assert!(locked <= Duration::from_millis(50), "{locked:?}");
    wire.run(Duration::from_millis(175));
    assert_eq!(
        wire.follower.relays[1]
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(leader_identity())
    );
    wire.run(Duration::from_millis(500));
    let error = wire.error().expect("following");
    assert!(error.abs() < 50, "{error} ns off");
    assert!(wire.leader.relays[1].is_grandmaster());
    assert_eq!(wire.follower.relays[1].role(1), Some(Role::TimeReceiver));
    // The leader's Announce names it to the follower within 125 ms.
    assert_eq!(
        wire.follower.relays[1]
            .grandmaster()
            .map(|grandmaster| grandmaster.clock),
        Some(leader_identity())
    );
}

#[test]
fn a_working_clock_is_arbitrary_time_on_its_own_domain_and_intervals() {
    let mut wire = Wire::new(true, (Role::TimeTransmitter, Role::TimeReceiver));
    wire.run(Duration::from_millis(500));
    let led: Vec<&Heard> = wire
        .heard
        .iter()
        .filter(|heard| heard.from_leader && heard.domain == 1)
        .collect();
    for message_type in [
        MessageType::Sync,
        MessageType::FollowUp,
        MessageType::Announce,
    ] {
        let sent: Vec<&&Heard> = led
            .iter()
            .filter(|heard| heard.message_type == message_type)
            .collect();
        assert!(!sent.is_empty(), "{message_type:?}");
        assert!(
            sent.iter()
                .all(|heard| heard.flags & Flags::PTP_TIMESCALE == 0),
            "{message_type:?} says PTP's timescale"
        );
    }
    assert!(
        led.iter()
            .filter(|heard| heard.message_type == MessageType::Sync)
            .all(|heard| heard.log_interval == -7)
    );
    let syncs = led
        .iter()
        .filter(|heard| heard.message_type == MessageType::Sync)
        .count();
    // 128 a second, from the link's first 40 ms or so on.
    assert!((55..=64).contains(&syncs), "{syncs} Syncs");
    // Its links measured by CMLDS, 32 times a second.
    let requests: Vec<&Heard> = wire
        .heard
        .iter()
        .filter(|heard| {
            heard.from_leader
                && heard.message_type == MessageType::PdelayReq
                && heard.major_sdo_id == link::CMLDS
        })
        .collect();
    assert!(
        requests
            .iter()
            .all(|heard| heard.domain == 0 && heard.log_interval == -5)
    );
    assert!((15..=17).contains(&requests.len()), "{}", requests.len());
}

#[test]
fn domain_0_runs_as_before_beside_a_working_clock() {
    let mut beside = Wire::new(true, (Role::TimeTransmitter, Role::TimeReceiver));
    let mut alone = Wire::new(false, (Role::Disabled, Role::Disabled));
    beside.run(Duration::from_secs(6));
    alone.run(Duration::from_secs(6));
    let domain_0 = |wire: &Wire| -> Vec<(bool, MessageType, i8)> {
        wire.heard
            .iter()
            .filter(|heard| heard.domain == 0 && heard.major_sdo_id == 1)
            .map(|heard| (heard.from_leader, heard.message_type, heard.log_interval))
            .collect()
    };
    assert_eq!(domain_0(&beside), domain_0(&alone));
    for wire in [&beside, &alone] {
        let relay = &wire.follower.relays[0];
        assert!(relay.as_capable(1));
        assert_eq!(
            relay.link(1).map(|link| link.neighbor.clock),
            Some(leader_identity())
        );
    }
    // Neither domain takes the other's messages: domain 0's relays saw
    // no Sync, being neither grandmaster capable.
    assert!(beside.follower.relays[0].grandmaster().is_none());
}

#[test]
fn a_port_given_no_role_carries_nothing_of_the_working_clock() {
    let mut wire = Wire::new(true, (Role::Disabled, Role::TimeReceiver));
    wire.run(Duration::from_millis(500));
    assert!(
        !wire
            .heard
            .iter()
            .any(|heard| heard.from_leader
                && (heard.domain == 1 || heard.major_sdo_id == link::CMLDS))
    );
    assert!(!wire.follower.relays[1].as_capable(1));
    assert!(wire.error().is_none());
}

#[test]
fn a_system_with_a_timereceiver_port_never_leads() {
    // Both configured to receive: neither is grandmaster, nor sends Sync.
    let mut wire = Wire::new(true, (Role::TimeReceiver, Role::TimeReceiver));
    wire.run(Duration::from_millis(500));
    for system in [&wire.leader, &wire.follower] {
        assert!(!system.relays[1].is_grandmaster());
        assert!(system.relays[1].as_capable(1));
    }
    assert!(
        !wire
            .heard
            .iter()
            .any(|heard| heard.domain == 1 && heard.message_type == MessageType::Sync)
    );
}
