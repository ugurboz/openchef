//! `openchef mcp ask-user`: the stdio MCP server agents' CLIs start (D-024). It is
//! hidden: OpenChef runs it, people do not.

use std::path::PathBuf;
use std::sync::Arc;

use openchef_api::mcp::{Asker, serve};
use openchef_api::{AskParams, Client};
use openchef_config::Paths;
use openchef_runtime::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::cli::AskUserArgs;
use crate::commands::daemon::client_runtime;
use crate::{Deps, exit};

/// The client name the daemon sees for questions.
const MCP_CLIENT_NAME: &str = "openchef-mcp";

pub(crate) fn ask_user(deps: &Deps, paths: &Paths, args: &AskUserArgs) -> anyhow::Result<i32> {
    let (reader, writer) = match &deps.mcp_stdio {
        Some(stdio) => stdio(),
        None => (
            Box::new(std::io::stdin()) as Box<dyn std::io::Read + Send>,
            Box::new(std::io::stdout()) as Box<dyn std::io::Write + Send>,
        ),
    };
    let asker = Arc::new(DaemonAsker {
        socket: paths.socket.clone(),
        task: args.task.clone(),
        agent: args.agent.clone(),
    });
    client_runtime()?.block_on(serve(reader, writer, asker, env!("CARGO_PKG_VERSION")))?;
    Ok(exit::OK)
}

/// Sends each question to the daemon's queue on its own connection, so a daemon
/// restart between questions does not break the server.
struct DaemonAsker {
    socket: PathBuf,
    task: String,
    agent: String,
}

impl Asker for DaemonAsker {
    fn ask(
        &self,
        question: String,
        options: Vec<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Option<String>, String>> {
        let socket = self.socket.clone();
        let params = AskParams {
            task: self.task.clone(),
            agent: self.agent.clone(),
            question,
            options,
        };
        Box::pin(async move {
            let c = Client::dial(&socket, MCP_CLIENT_NAME)
                .await
                .map_err(|e| format!("reach the daemon: {e}"))?;
            // Closing the connection is what tells the daemon the agent stopped
            // waiting: it withdraws the question.
            let asked = tokio::select! {
                r = c.ask(&params) => Some(r),
                () = cancel.cancelled() => None,
            };
            c.close();
            match asked {
                Some(Ok(r)) => Ok(r.answer),
                Some(Err(e)) => Err(format!("ask: {e}")),
                None => Err("the agent stopped waiting".to_owned()),
            }
        })
    }
}
