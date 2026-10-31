//! Prints what each interface does about EEE and PAUSE, and whether it
//! filters VLANs in hardware.

#[cfg(target_os = "linux")]
fn main() {
    for interface in raw_ether::interfaces() {
        if let Ok(power) = raw_ether::interfaces::link_power(&interface.name) {
            let vlans = raw_ether::interfaces::filters_vlans(&interface.name);
            println!(
                "{}: EEE {}, PAUSE {}, VLAN filter {:?}",
                interface.name, power.eee, power.pause, vlans
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("only on Linux");
}
