use ockam_core::compat::sync::Arc;
use ockam_core::errcode::{Kind, Origin};
use ockam_core::flow_control::FlowControls;
use ockam_core::{Error, Result, Route, TransportType};
use ockam_transport_core::Transport;

use crate::Context;

impl Context {
    /// Return the list of supported transports
    pub fn register_transport(&self, transport: Arc<dyn Transport>) {
        let mut transports = self.transports.lock().unwrap();
        transports.insert(transport.transport_type(), transport);
    }

    /// Return true if a given transport has already been registered
    pub fn is_transport_registered(&self, transport_type: TransportType) -> bool {
        let transports = self.transports.lock().unwrap();
        transports.contains_key(&transport_type)
    }

    /// For each address handled by a given transport in a route, for example, (TCP, "127.0.0.1:4000")
    /// Create a worker supporting the routing of messages for this transport and replace the address
    /// in the route with the worker address
    pub async fn resolve_transport_route(
        &self,
        flow_controls: &FlowControls,
        route: Route,
    ) -> Result<Route> {
        let transports = self.transports.lock().unwrap().clone();
        let mut resolved = route;
        for transport in transports.values() {
            resolved = transport.resolve_route(flow_controls, resolved).await?;
            if resolved.is_local() {
                return Ok(resolved);
            }
        }

        // If eventually some addresses could not be resolved return an error
        Err(Error::new(
            Origin::Transport,
            Kind::NotFound,
            format!(
                "the route {:?} could not be fully resolved to local addresses",
                {}
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use ockam_core::{async_trait, route, Address, LOCAL};

    use super::*;

    #[ockam_macros::test(crate = "crate")]
    async fn test_transports(ctx: &mut Context) -> Result<()> {
        let transport = Arc::new(SomeTransport());
        ctx.register_transport(transport.clone());
        assert!(ctx.is_transport_registered(transport.transport_type()));
        ctx.stop().await
    }

    #[ockam_macros::test(crate = "crate")]
    async fn test_resolve_route(ctx: &mut Context) -> Result<()> {
        let transport = Arc::new(SomeTransport());
        ctx.register_transport(transport.clone());

        let flow_controls = FlowControls::default();

        // resolve a route with known transports
        let result = ctx
            .resolve_transport_route(
                &flow_controls,
                route![(transport.transport_type(), "address")],
            )
            .await;
        assert!(result.is_ok());

        // resolve a route with unknown transports
        let result = ctx
            .resolve_transport_route(&flow_controls, route![(TransportType::new(1), "address")])
            .await;

        assert!(result.is_err());
        ctx.stop().await
    }

    struct SomeTransport();

    #[async_trait]
    impl Transport for SomeTransport {
        fn transport_type(&self) -> TransportType {
            TransportType::new(10)
        }

        /// This implementation simply marks each address as a local address
        async fn resolve_route(
            &self,
            _flow_controls: &FlowControls,
            route: Route,
        ) -> Result<Route> {
            let mut result = Route::new();
            for address in route.iter() {
                if address.transport_type() == self.transport_type() {
                    result = result.append(Address::new(LOCAL, address.address()));
                } else {
                    result = result.append(address.clone());
                }
            }

            let resolved = result.into();
            Ok(resolved)
        }
    }
}