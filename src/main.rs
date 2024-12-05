use anyhow::{Context, Result};
use niri_ipc::{Action, Reply, Request, Response, Window, socket::Socket};
use std::{fs, path::PathBuf, sync::Arc};
use tokio::select;
use tokio::signal::unix::{SignalKind, signal};
use tokio::spawn;
use tokio::sync::Notify;

/// Fetch the list of windows.
async fn get_niri_windows() -> Result<Vec<Window>> {
    let socket = Socket::connect().context("failed to connect to niri ipc socket")?;
    let (reply, _event) = socket
        .send(Request::Windows)
        .context("failed to retrieve windows from niri ipc")?;

    match reply {
        Reply::Ok(Response::Windows(windows)) => Ok(windows),
        Reply::Err(error_msg) => anyhow::bail!("niri ipc returned an error: {}", error_msg),
        _ => anyhow::bail!("Unexpected reply type from niri"),
    }
}

fn get_session_file_path() -> Result<PathBuf> {
    let mut session_dir =
        dirs::data_dir().context("Failed to locate data directory (XDG_DATA_HOME)")?;
    session_dir.push("niri-session-manager");
    fs::create_dir_all(&session_dir).context("Failed to create session directory")?;
    Ok(session_dir.join("session.json"))
}

async fn save_session(file_path: &PathBuf) -> Result<()> {
    let windows = get_niri_windows().await?;
    let json_data =
        serde_json::to_string_pretty(&windows).context("Failed to serialize window data")?;
    fs::write(file_path, json_data).context("Failed to write session file")?;
    println!("Session saved to {}", file_path.display());
    Ok(())
}

/// Restore a session state from a file.
async fn restore_session(file_path: &PathBuf) -> Result<()> {
    if !file_path.exists() {
        println!("No previous session found at {}", file_path.display());
        return Ok(());
    }

    let session_data = fs::read_to_string(file_path).context("Failed to read session file")?;
    let windows: Vec<Window> =
        serde_json::from_str(&session_data).context("Failed to parse session JSON")?;

    let mut handles = vec![];

    for window in &windows {
        // probably only useful when restarting systemd service
        if let Some(app_id) = &window.app_id {
            if get_niri_windows()
                .await?
                .iter()
                .any(|w| w.app_id == Some(app_id.clone()))
            {
                continue;
            }
        }

        let app_id = window.app_id.clone().unwrap_or_else(|| "".to_string());
        let workspace_id = window.workspace_id;
        let handle = spawn(async move {
            let spawn_socket = Socket::connect().context("failed to connect to niri ipc socket")?;
            let (spawn_reply, _event) = spawn_socket
                .send(Request::Action(Action::Spawn {
                    command: vec![app_id.clone()],
                }))
                .context("Failed to send spawn request")?;

            if let Reply::Ok(Response::Handled) = spawn_reply {
                let move_socket =
                    Socket::connect().context("failed to connect to niri ipc socket")?;
                for _ in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Ok(current_windows) = get_niri_windows().await {
                        if let Some(new_window) = current_windows
                            .iter()
                            .find(|w| w.app_id == Some(app_id.clone()))
                        {
                            let move_request = Request::Action(Action::MoveWindowToWorkspace {
                                window_id: Some(new_window.id),
                                reference: niri_ipc::WorkspaceReferenceArg::Id(
                                    workspace_id.expect("no workspace id"),
                                ),
                            });
                            let _ = move_socket
                                .send(move_request)
                                .context("Failed to move window to the workspace")?;
                            break;
                        }
                    }
                }
            } else {
                println!("Failed to spawn app: {:?}", app_id);
            }
            Result::<()>::Ok(())
        });
        handles.push(handle);
    }

    // Wait for all window spawning and moving tasks to complete
    for handle in handles {
        let _ = handle.await.context("Failed to join handle")?;
    }

    println!("Session restored.");
    Ok(())
}

/// Handle shutdown signals and notify the main function.
async fn handle_shutdown_signals(shutdown_signal: Arc<Notify>) {
    let mut term_signal = signal(SignalKind::terminate()).expect("Failed to listen for SIGTERM");
    let mut int_signal = signal(SignalKind::interrupt()).expect("Failed to listen for SIGINT");
    let mut quit_signal = signal(SignalKind::quit()).expect("Failed to listen for SIGQUIT");

    select! {
        _ = term_signal.recv() => shutdown_signal.notify_one(),
        _ = int_signal.recv() => shutdown_signal.notify_one(),
        _ = quit_signal.recv() => shutdown_signal.notify_one(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let session_file_path = get_session_file_path()?;
    let shutdown_signal = Arc::new(Notify::new());

    restore_session(&session_file_path).await?;

    handle_shutdown_signals(Arc::clone(&shutdown_signal)).await;

    // Wait for SIGTERM, SIGINT, or SIGQUIT
    shutdown_signal.notified().await;

    save_session(&session_file_path).await?;

    Ok(())
}
