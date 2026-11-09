//! Answers a station's FTM Requests on an access point, as gptpd's 802.11
//! port does, logging each frame and its stamps: a test that FTM frames
//! come to a program other than hostapd (hostapd's `ftm_external=1`) and
//! that a station measures from the burst it is sent.
//!
//! ftm_answer INTERFACE [STATION] [--follow-up] [--bias-ns N]
//!
//! The first station associated is answered unless one is named. Each
//! frame carries the times of the frame before; with `--follow-up` also a
//! blank Follow_Up element (802.1AS-2020 12.7), else none. `--bias-ns`
//! reports each Ack N ns later than it came, as an ESP responder's offset
//! does, for a station too near to measure a round trip above 0.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use gptp::dot11::FOLLOW_UP_LENGTH;
use gptp::dot11::ftm::{
    ACTION_FTM_REQUEST, CATEGORY_PUBLIC, FineTimingMeasurement, FtmRequest, Responder,
};
use raw_ether::MacAddress;
use raw_ether::stamped::wait_any_with;
use raw_ether::wifi::{Event, Radio};

/// How long to wait for the radio when no frame is due.
const IDLE_WAIT: Duration = Duration::from_millis(500);

fn main() {
    if let Err(error) = run() {
        eprintln!("ftm_answer: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let interface = arguments
        .next()
        .ok_or("usage: ftm_answer INTERFACE [STATION] [--follow-up] [--bias-ns N]")?;
    let mut named = None;
    let mut follow_up = false;
    let mut bias_ns: i128 = 0;
    while let Some(argument) = arguments.next() {
        if argument == "--follow-up" {
            follow_up = true;
        } else if argument == "--bias-ns" {
            let value = arguments.next().ok_or("--bias-ns needs a value")?;
            bias_ns = value
                .parse()
                .map_err(|error| format!("--bias-ns {value}: {error}"))?;
        } else {
            named = Some(
                argument
                    .parse::<MacAddress>()
                    .map_err(|error| format!("{argument}: {error:?}"))?,
            );
        }
    }
    let radio = Radio::open(&interface).map_err(|error| format!("{interface}: {error}"))?;
    if !radio
        .access_point()
        .map_err(|error| format!("{interface}: {error}"))?
    {
        return Err(format!("{interface} is not an access point"));
    }
    radio
        .register_action(&[CATEGORY_PUBLIC, ACTION_FTM_REQUEST])
        .map_err(|error| {
            format!(
                "{interface}: FTM Requests do not come here ({error}): another program, hostapd \
                 without ftm_external=1, takes them"
            )
        })?;
    println!("{interface}: FTM Requests come here");
    let station = match named {
        Some(station) => station,
        None => *radio
            .stations()
            .map_err(|error| format!("{interface}: {error}"))?
            .first()
            .ok_or_else(|| format!("{interface}: no station associated"))?,
    };
    radio
        .set_hw_timestamp(station, true)
        .map_err(|error| format!("stamping {station}'s frames: {error}"))?;
    println!("answering {station}, its frames stamped, Acks reported {bias_ns} ns late");

    let start = Instant::now();
    let mut responder = Responder::new();
    // Frames sent whose status is still to come: their cookie and token.
    let mut pending: Vec<(u64, u8)> = Vec::new();
    let send = |frame: &FineTimingMeasurement, pending: &mut Vec<(u64, u8)>| match radio
        .send_action(station, radio.address(), &frame.encode())
    {
        Ok(cookie) => {
            println!(
                "{:10.3} sent FTM {} following up {}, tod {} toa {} ps{}",
                start.elapsed().as_secs_f64(),
                frame.dialog_token,
                frame.follow_up_dialog_token,
                frame.tod,
                frame.toa,
                frame
                    .parameters
                    .map(|granted| format!(", granted {granted:?}"))
                    .unwrap_or_default()
            );
            pending.push((cookie, frame.dialog_token));
        }
        Err(error) => println!("sending FTM {}: {error}", frame.dialog_token),
    };
    loop {
        let timeout = responder
            .poll_timeout()
            .map(|due| due.saturating_sub(start.elapsed()))
            .unwrap_or(IDLE_WAIT)
            .min(IDLE_WAIT);
        wait_any_with(&[], &[radio.as_fd()], Some(timeout))
            .map_err(|error| format!("waiting: {error}"))?;
        while let Some(event) = radio.take().map_err(|error| format!("reading: {error}"))? {
            match event {
                Event::Frame { source, action, .. } if source == station => {
                    match FtmRequest::decode(&action) {
                        Ok(request) => {
                            println!(
                                "{:10.3} FTM Request, trigger {}{}",
                                start.elapsed().as_secs_f64(),
                                request.trigger,
                                request
                                    .parameters
                                    .map(|asked| format!(", asking {asked:?}"))
                                    .unwrap_or_default()
                            );
                            if let Some(frame) = responder.request(start.elapsed(), &request) {
                                send(&frame, &mut pending);
                            }
                        }
                        Err(error) => println!("an FTM Request not understood: {error:?}"),
                    }
                }
                Event::Frame { .. } => {}
                Event::TxStatus {
                    cookie,
                    acked,
                    sent,
                    ack_received,
                } => {
                    let Some(position) = pending.iter().position(|(each, _)| *each == cookie)
                    else {
                        continue;
                    };
                    let (_, token) = pending.remove(position);
                    println!(
                        "{:10.3} FTM {token}: {}, left {}, Ack {}",
                        start.elapsed().as_secs_f64(),
                        if acked { "acked" } else { "not acked" },
                        sent.map_or("unstamped".to_owned(), |at| format!("{at} ns")),
                        ack_received.map_or("unstamped".to_owned(), |at| format!("{at} ns")),
                    );
                    if let (true, Some(departure), Some(ack_arrival)) = (acked, sent, ack_received)
                    {
                        responder.confirmed(token, departure, ack_arrival + bias_ns);
                        if follow_up {
                            responder.set_follow_up(token, &[0; FOLLOW_UP_LENGTH]);
                        }
                    }
                }
                Event::Station { peer, joined } => {
                    println!(
                        "{:10.3} {peer} {}",
                        start.elapsed().as_secs_f64(),
                        if joined { "joined" } else { "left" }
                    );
                    if peer == station && joined {
                        responder.reset();
                        if let Err(error) = radio.set_hw_timestamp(station, true) {
                            println!("stamping {station}'s frames: {error}");
                        }
                    }
                }
            }
        }
        if let Some(frame) = responder.poll_frame(start.elapsed()) {
            send(&frame, &mut pending);
        }
    }
}
