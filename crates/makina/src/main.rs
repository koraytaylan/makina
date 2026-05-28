#[tokio::main]
async fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!("Makina v{version} — multi-agent software-factory orchestrator");
}
