use anyhow::Result;

use crate::auth::{check_auth, launch_login};
use crate::types::BackendId;

pub async fn run(backend: BackendId, check_only: bool) -> Result<()> {
    if check_only {
        let status = check_auth(backend).await;
        if status.ok {
            println!("[{backend}] authenticated");
        } else {
            println!("[{backend}] NOT authenticated.");
            println!("{}", status.hint);
            println!("Login command: {}", status.login_command);
        }
        return Ok(());
    }

    let (status, launch) = launch_login(backend).await;
    if status.ok {
        println!("[{backend}] already authenticated.");
        return Ok(());
    }
    println!("[{backend}] is not authenticated.");
    println!("{}", status.hint);
    println!("Login command: {}", status.login_command);
    if let Some(lo) = launch {
        println!();
        println!("{}", lo.message);
        if lo.launched {
            println!(
                "Complete the OAuth flow in the new Terminal window, then retry your command."
            );
        }
    }
    Ok(())
}
