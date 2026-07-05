use agentfs_core::AgentFSOptions;

use crate::cmd::init::open_agentfs;

pub async fn handle_pull_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = open_agentfs(options).await?;
    agent.pull().await?;
    eprintln!("Remote data pulled to local db successfully");
    Ok(())
}

pub async fn handle_push_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = open_agentfs(options).await?;
    agent.push().await?;
    eprintln!("Local data pushed to remote db successfully");
    Ok(())
}

pub async fn handle_checkpoint_command(id_or_path: String) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = open_agentfs(options).await?;
    agent.checkpoint().await?;
    eprintln!("Local db checkpointed successfully");
    Ok(())
}

pub async fn handle_stats_command(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
) -> anyhow::Result<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = open_agentfs(options).await?;
    let result: anyhow::Result<()> = async {
        let stats = agent.sync_stats().await?;
        stdout.write_all(serde_json::to_string(&stats)?.as_bytes())?;
        Ok(())
    }
    .await;
    crate::cmd::init::finalize_readonly(&agent).await;
    result
}
