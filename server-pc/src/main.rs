use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    remotecontrol_server::run_server().await
}
