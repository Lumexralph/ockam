use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;

use clap::Args;
use colorful::Colorful;
use miette::{miette, IntoDiagnostic};
use miette::{Result, WrapErr};
use tokio::sync::Mutex;
use tokio::try_join;
use tracing::trace;

use ockam::identity::Identifier;
use ockam::Context;
use ockam_api::address::extract_address_value;
use ockam_api::cli_state::CliState;
use ockam_api::nodes::models::portal::InletStatus;
use ockam_api::nodes::service::portals::Inlets;
use ockam_api::nodes::BackgroundNode;
use ockam_core::api::{Reply, Status};
use ockam_core::errcode::{Kind, Origin};
use ockam_core::Error;
use ockam_multiaddr::proto::Project;
use ockam_multiaddr::{MultiAddr, Protocol as _};

use crate::tcp::util::alias_parser;
use crate::terminal::OckamColor;
use crate::util::duration::duration_parser;
use crate::util::parsers::socket_addr_parser;
use crate::util::{find_available_port, node_rpc, port_is_free_guard, process_nodes_multiaddr};
use crate::{display_parse_logs, docs, fmt_log, fmt_ok, CommandGlobalOpts};

const AFTER_LONG_HELP: &str = include_str!("./static/create/after_long_help.txt");

/// Create TCP Inlets
#[derive(Clone, Debug, Args)]
#[command(after_long_help = docs::after_help(AFTER_LONG_HELP))]
pub struct CreateCommand {
    /// Node on which to start the tcp inlet.
    #[arg(long, display_order = 900, id = "NODE", value_parser = extract_address_value)]
    at: Option<String>,

    /// Address on which to accept tcp connections.
    #[arg(long, display_order = 900, id = "SOCKET_ADDRESS", hide_default_value = true, default_value_t = default_from_addr(), value_parser = socket_addr_parser)]
    from: SocketAddr,

    /// Route to a tcp outlet. Can be a full route or the name of an existing relay
    #[arg(long, display_order = 900, id = "ROUTE", default_value_t = default_to_addr())]
    to: String,

    /// Authorized identity for secure channel connection
    #[arg(long, name = "AUTHORIZED", display_order = 900)]
    authorized: Option<Identifier>,

    /// Assign a name to this inlet.
    #[arg(long, display_order = 900, id = "ALIAS", value_parser = alias_parser)]
    alias: Option<String>,

    /// Time to wait for the outlet to be available.
    #[arg(long, display_order = 900, id = "WAIT", default_value = "5s", value_parser = duration_parser)]
    connection_wait: Duration,

    /// Time to wait before retrying to connect to outlet.
    #[arg(long, display_order = 900, id = "RETRY", default_value = "20s", value_parser = duration_parser)]
    retry_wait: Duration,

    /// Override default timeout
    #[arg(long, value_parser = duration_parser)]
    timeout: Option<Duration>,
}

pub(crate) fn default_from_addr() -> SocketAddr {
    let port = find_available_port().expect("Failed to find available port");
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
}

fn default_to_addr() -> String {
    "/project/$PROJECT_NAME/service/forward_to_$RELAY_NAME/secure/api/service/outlet".to_string()
}

impl CreateCommand {
    pub fn run(self, opts: CommandGlobalOpts) {
        node_rpc(rpc, (opts, self));
    }

    fn to(&self) -> MultiAddr {
        MultiAddr::from_str(&self.to).unwrap()
    }

    async fn parse_args(mut self, opts: &CommandGlobalOpts) -> Result<Self> {
        let default_project_name = &opts
            .state
            .get_default_project()
            .await
            .ok()
            .map(|p| p.name());

        self.to = Self::parse_arg_to(&opts.state, self.to, default_project_name).await?;
        Ok(self)
    }

    async fn parse_arg_to(
        state: &CliState,
        to: impl Into<String>,
        default_project_name: &Option<String>,
    ) -> Result<String> {
        let mut to = to.into();
        let missing_project_error =
            "There is no default project defined. Please enroll or create a project.";

        // Replace the placeholders in the default arg value
        if to.starts_with("/project/") {
            let project_name = default_project_name
                .clone()
                .ok_or(miette!(missing_project_error))?;
            to = to.replace("$PROJECT_NAME", &project_name);
            to = to.replace("$RELAY_NAME", "default");
        }

        // Parse the address
        let ma = match MultiAddr::from_str(&to) {
            // The user provided a full route
            Ok(ma) => ma,
            // The user provided the name of the relay
            Err(_) => {
                if to.contains('/') {
                    return Err(miette!("The relay name can't contain '/'"));
                }
                let project_name = default_project_name
                    .clone()
                    .ok_or(miette!(missing_project_error))?;

                MultiAddr::from_str(&format!(
                    "/project/{}/service/forward_to_{to}/secure/api/service/outlet",
                    project_name
                ))
                .into_diagnostic()
                .wrap_err("Invalid address value or relay name")?
            }
        };
        Ok(process_nodes_multiaddr(&ma, state).await?.to_string())
    }
}

async fn rpc(ctx: Context, (opts, cmd): (CommandGlobalOpts, CreateCommand)) -> Result<()> {
    let cmd = cmd.parse_args(&opts).await?;
    opts.terminal.write_line(&fmt_log!(
        "Creating TCP Inlet at {}...\n",
        cmd.from
            .to_string()
            .color(OckamColor::PrimaryResource.color())
    ))?;
    display_parse_logs(&opts);

    let mut node = BackgroundNode::create(&ctx, &opts.state, &cmd.at).await?;
    cmd.timeout.map(|t| node.set_timeout(t));

    let is_finished: Mutex<bool> = Mutex::new(false);
    let progress_bar = opts.terminal.progress_spinner();
    let create_inlet = async {
        port_is_free_guard(&cmd.from)?;
        if cmd.to().matches(0, &[Project::CODE.into()]) && cmd.authorized.is_some() {
            return Err(miette!("--authorized can not be used with project addresses").into());
        }

        let inlet = loop {
            let result: Reply<InletStatus> = node
                .create_inlet(
                    &ctx,
                    &cmd.from.to_string(),
                    &cmd.to(),
                    &cmd.alias,
                    &cmd.authorized,
                    cmd.connection_wait,
                )
                .await?;

            match result {
                Reply::Successful(inlet_status) => {
                    *is_finished.lock().await = true;
                    break inlet_status;
                }
                Reply::Failed(e, s) => {
                    if let Some(status) = s {
                        if status == Status::BadRequest {
                            Err(Error::new(
                                Origin::Api,
                                Kind::Invalid,
                                e.message().unwrap_or("bad request when creating an inlet"),
                            ))?
                        }
                    };
                    trace!("the inlet creation returned a non-OK status: {s:?}");

                    if cmd.retry_wait.as_millis() == 0 {
                        return Err(miette!("Failed to create TCP inlet"))?;
                    }

                    if let Some(spinner) = progress_bar.as_ref() {
                        spinner.set_message(format!(
                            "Waiting for inlet {} to be available... Retrying momentarily",
                            &cmd.to
                                .to_string()
                                .color(OckamColor::PrimaryResource.color())
                        ));
                    }
                    tokio::time::sleep(cmd.retry_wait).await
                }
            }
        };

        Ok(inlet)
    };

    let progress_messages = vec![
        format!(
            "Creating TCP Inlet on {}...",
            &node.node_name().color(OckamColor::PrimaryResource.color())
        ),
        format!(
            "Hosting TCP Socket at {}...",
            &cmd.from
                .to_string()
                .color(OckamColor::PrimaryResource.color())
        ),
        format!(
            "Establishing connection to outlet {}...",
            &cmd.to
                .to_string()
                .color(OckamColor::PrimaryResource.color())
        ),
    ];
    let progress_output = opts.terminal.progress_output_with_progress_bar(
        &progress_messages,
        &is_finished,
        progress_bar.as_ref(),
    );
    let (inlet, _) = try_join!(create_inlet, progress_output)?;
    opts.terminal
        .stdout()
        .plain(
            fmt_ok!(
                "TCP Inlet {} on node {} is now sending traffic\n",
                &cmd.from
                    .to_string()
                    .color(OckamColor::PrimaryResource.color()),
                &node.node_name().color(OckamColor::PrimaryResource.color())
            ) + &fmt_log!(
                "to the outlet at {}",
                &cmd.to
                    .to_string()
                    .color(OckamColor::PrimaryResource.color())
            ),
        )
        .machine(inlet.bind_addr.to_string())
        .json(serde_json::json!(&inlet))
        .write_line()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use miette::Result;

    use super::*;

    #[tokio::test]
    async fn test_parse_arg_to() -> Result<()> {
        let state = CliState::test().await?;
        let default_project_name = Some("p1".to_string());

        // Invalid values
        CreateCommand::parse_arg_to(&state, "/alice/service", &default_project_name)
            .await
            .expect_err("Invalid protocol");
        CreateCommand::parse_arg_to(&state, "alice/forwarder", &default_project_name)
            .await
            .expect_err("Invalid protocol");

        // The placeholders are replaced in the default value
        let res =
            CreateCommand::parse_arg_to(&state, default_to_addr(), &default_project_name).await?;
        assert_eq!(
            res,
            "/project/p1/service/forward_to_default/secure/api/service/outlet"
        );

        // The user provides a full project route
        let addr = "/project/p1/service/forward_to_n1/secure/api/service/outlet";
        let res = CreateCommand::parse_arg_to(&state, addr, &default_project_name).await?;
        assert_eq!(res, addr);

        // The user provides the name of the relay
        let res = CreateCommand::parse_arg_to(&state, "alice", &default_project_name).await?;
        assert_eq!(
            res,
            "/project/p1/service/forward_to_alice/secure/api/service/outlet"
        );
        Ok(())
    }
}
