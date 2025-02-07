use std::str::FromStr;

use either::Either;
use miette::IntoDiagnostic;
use minicbor::Decoder;

use ockam::identity::models::CredentialAndPurposeKey;
use ockam::Result;
use ockam_core::api::{Error, Request, RequestHeader, Response};
use ockam_core::async_trait;
use ockam_multiaddr::MultiAddr;
use ockam_node::Context;

use crate::cloud::AuthorityNode;
use crate::error::ApiError;
use crate::local_multiaddr_to_route;
use crate::nodes::models::credentials::{GetCredentialRequest, PresentCredentialRequest};
use crate::nodes::BackgroundNode;

use super::NodeManagerWorker;

#[async_trait]
pub trait Credentials {
    async fn authenticate(
        &self,
        ctx: &Context,
        identity_name: Option<String>,
    ) -> miette::Result<()> {
        let _ = self.get_credential(ctx, false, identity_name).await?;
        Ok(())
    }

    async fn get_credential(
        &self,
        ctx: &Context,
        overwrite: bool,
        identity_name: Option<String>,
    ) -> miette::Result<CredentialAndPurposeKey>;

    async fn present_credential(
        &self,
        ctx: &Context,
        to: &MultiAddr,
        oneway: bool,
    ) -> miette::Result<()>;
}

#[async_trait]
impl Credentials for AuthorityNode {
    async fn get_credential(
        &self,
        ctx: &Context,
        overwrite: bool,
        identity_name: Option<String>,
    ) -> miette::Result<CredentialAndPurposeKey> {
        let body = GetCredentialRequest::new(overwrite, identity_name);
        let req = Request::post("/node/credentials/actions/get").body(body);
        self.secure_client
            .ask(ctx, "", req)
            .await
            .into_diagnostic()?
            .success()
            .into_diagnostic()
    }

    async fn present_credential(
        &self,
        ctx: &Context,
        to: &MultiAddr,
        oneway: bool,
    ) -> miette::Result<()> {
        let body = PresentCredentialRequest::new(to, oneway);
        let req = Request::post("/node/credentials/actions/present").body(body);
        self.secure_client
            .tell(ctx, "", req)
            .await
            .into_diagnostic()?
            .success()
            .into_diagnostic()
    }
}

#[async_trait]
impl Credentials for BackgroundNode {
    async fn get_credential(
        &self,
        ctx: &Context,
        overwrite: bool,
        identity_name: Option<String>,
    ) -> miette::Result<CredentialAndPurposeKey> {
        let body = GetCredentialRequest::new(overwrite, identity_name);
        self.ask(
            ctx,
            Request::post("/node/credentials/actions/get").body(body),
        )
        .await
    }

    async fn present_credential(
        &self,
        ctx: &Context,
        to: &MultiAddr,
        oneway: bool,
    ) -> miette::Result<()> {
        let body = PresentCredentialRequest::new(to, oneway);
        self.tell(
            ctx,
            Request::post("/node/credentials/actions/present").body(body),
        )
        .await
    }
}

impl NodeManagerWorker {
    pub(super) async fn get_credential(
        &mut self,
        req: &RequestHeader,
        dec: &mut Decoder<'_>,
        ctx: &Context,
    ) -> Result<Either<Response<Error>, Response<CredentialAndPurposeKey>>> {
        let request: GetCredentialRequest = dec.decode()?;

        let identifier = self
            .node_manager
            .get_identifier_by_name(request.identity_name)
            .await?;

        match self
            .node_manager
            .get_credential(ctx, &identifier, None)
            .await
        {
            Ok(Some(c)) => Ok(Either::Right(Response::ok(req).body(c))),
            Ok(None) => Ok(Either::Left(Response::not_found(
                req,
                &format!("no credential found for {}", identifier),
            ))),
            Err(e) => Ok(Either::Left(Response::internal_error(
                req,
                &format!(
                    "Error retrieving credential from authority for {}: {}",
                    identifier, e,
                ),
            ))),
        }
    }

    pub(super) async fn present_credential(
        &self,
        req: &RequestHeader,
        dec: &mut Decoder<'_>,
        ctx: &Context,
    ) -> Result<Response, Response<Error>> {
        let request: PresentCredentialRequest = dec.decode()?;

        // TODO: Replace with self.connect?
        let route = MultiAddr::from_str(&request.route).map_err(|_| {
            ApiError::core(format!(
                "Couldn't convert String to MultiAddr: {}",
                &request.route
            ))
        })?;
        let route = local_multiaddr_to_route(&route)?;

        let identifier = self.node_manager.identifier();
        let credential = self
            .node_manager
            .get_credential(ctx, &identifier, None)
            .await?
            .unwrap_or_else(|| panic!("A credential must be retrieved for {}", identifier));

        if request.oneway {
            self.node_manager
                .credentials_service()
                .present_credential(ctx, route, credential)
                .await?;
        } else {
            self.node_manager
                .credentials_service()
                .present_credential_mutual(
                    ctx,
                    route,
                    &self.node_manager.trust_context()?.authorities(),
                    credential,
                )
                .await?;
        }

        let response = Response::ok(req);
        Ok(response)
    }
}
