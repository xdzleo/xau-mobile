//! Which `DeviceType` values will Xbox mint a device token for?
//!
//! Presence is reported against `devices/current`, with no device field
//! in the heartbeat body — so what a profile looks like it is playing ON
//! is decided by the device token behind the session, established at
//! sign-in. Microsoft Rewards has separate PC and console activities
//! (`PCGamePlay` vs `playConsole`), so this is the difference between
//! farming one and the other.
//!
//! The device-token call is the FIRST step of the SISU flow and needs no
//! Microsoft account token — only a ProofKey we generate. That makes
//! this probe safe: it mints throwaway device tokens and touches no
//! account state.
//!
//!     cargo run -p xau-core --example probe_device_types

use xau_core::oauth::sisu::XboxSignedClient;

#[tokio::main]
async fn main() {
    let candidates = [
        ("Win32", "0.0.0"),   // what we ship today
        ("Android", "0.0.0"), // common in community tooling
        ("iOS", "0.0.0"),
        ("Scarlett", "0.0.0"), // Xbox Series X|S
        ("XboxOne", "0.0.0"),
        ("Durango", "0.0.0"), // Xbox One codename
        ("Xbox360", "0.0.0"),
        ("Nintendo", "0.0.0"),
        ("WindowsOneCore", "0.0.0"),
    ];

    println!("{:<16} {:<10} {}", "DeviceType", "result", "detail");
    println!("{}", "-".repeat(78));

    for (dt, ver) in candidates {
        let client = match XboxSignedClient::new() {
            Ok(c) => c,
            Err(e) => {
                println!("{dt:<16} {:<10} {e}", "SETUP-ERR");
                continue;
            }
        };
        match client.request_device_token(dt, ver).await {
            Ok(resp) if !resp.token.is_empty() => {
                println!(
                    "{dt:<16} {:<10} token de {} chars",
                    "ACEITO",
                    resp.token.len()
                );
            }
            Ok(_) => println!("{dt:<16} {:<10} resposta sem token", "VAZIO"),
            Err(e) => {
                // Trim: Xbox error bodies are long and mostly boilerplate.
                let msg = e.to_string();
                let short: String = msg.chars().take(150).collect();
                println!("{dt:<16} {:<10} {short}", "RECUSADO");
            }
        }
    }
}
