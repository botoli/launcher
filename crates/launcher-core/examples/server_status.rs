//! Read-only probe: cargo run -p launcher-core --example server_status -- host:port

use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .ok_or("usage: server_status host[:port]")?;
    let latency = launcher_core::ping_minecraft_server(&address, Duration::from_secs(5)).await?;
    println!(
        "Minecraft server {address}: {} ms",
        latency.as_micros().div_ceil(1000).max(1)
    );
    Ok(())
}
