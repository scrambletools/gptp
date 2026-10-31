//! The helper an app ships for raw sockets, as small as it can be: grant
//! it CAP_NET_RAW (`sudo setcap cap_net_raw+ep target/debug/examples/netraw`)
//! and have the app start it with `raw_ether::broker::Broker::start` and
//! install it. The app itself then needs no capability.

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(raw_ether::broker::serve());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("only Linux grants raw sockets by capability");
    std::process::exit(2);
}
