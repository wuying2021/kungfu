use anyhow::Error;
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, UdpSocket},
    time,
};
use trust_dns_server::{
    authority::{
        AuthorityObject, Catalog, LookupError, LookupObject, LookupOptions, MessageRequest,
        UpdateResult, ZoneType,
    },
    client::rr::LowerName,
    proto::{op::ResponseCode, rr::RecordType},
    resolver::{
        config::{NameServerConfigGroup, ResolverOpts},
        Name,
    },
    server::{Request, RequestHandler, RequestInfo, ResponseHandler, ResponseInfo},
    store::forwarder::{ForwardAuthority, ForwardConfig},
    ServerFuture,
};

use super::dns_handler::DnsHandler;
use crate::config::ArcSetting;

pub(crate) async fn build_dns_server(setting: ArcSetting) -> Result<ServerFuture<Handler>, Error> {
    let dns_upstream = setting
        .dns_upstream
        .iter()
        .map(|v| v.parse().unwrap())
        .collect::<Vec<IpAddr>>();
    let name_servers = NameServerConfigGroup::from_ips_clear(&dns_upstream, 53, true);

    // optimize for forward / upstream
    let mut opts = ResolverOpts::default();
    opts.timeout = Duration::from_secs(2);
    opts.cache_size = 128;
    opts.positive_max_ttl = Some(Duration::from_secs(120));
    opts.positive_min_ttl = Some(Duration::from_secs(60));
    opts.negative_max_ttl = Some(Duration::from_secs(120));
    opts.negative_max_ttl = Some(Duration::from_secs(120));

    let forward_config = ForwardConfig {
        name_servers,
        options: Some(opts),
    };

    let upstream =
        ForwardAuthority::try_from_config(Name::root(), ZoneType::Forward, &forward_config)
            .await
            .unwrap();

    let upstream = Arc::new(upstream);
    let handler = DnsHandler::new(Box::new(upstream.clone()), setting.clone());
    let authority = HijackAuthority::new(Box::new(upstream.clone()), handler);

    let mut catalog = Catalog::new();
    catalog.upsert(LowerName::from(Name::root()), Box::new(authority));

    let mut server = ServerFuture::new(Handler { catalog });
    log::info!("dns listen port: {}", setting.dns_port);
    server.register_socket(UdpSocket::bind(format!("0.0.0.0:{}", setting.dns_port)).await?);
    server.register_listener(
        TcpListener::bind(format!("0.0.0.0:{}", setting.dns_port)).await?,
        Duration::from_secs(5),
    );

    Ok(server)
}

struct HijackAuthority {
    upstream: Box<dyn AuthorityObject>,
    handler: DnsHandler,
}

impl HijackAuthority {
    fn new(upstream: Box<dyn AuthorityObject>, handler: DnsHandler) -> Self {
        Self { upstream, handler }
    }
}

#[async_trait::async_trait]
impl AuthorityObject for HijackAuthority {
    fn box_clone(&self) -> Box<dyn AuthorityObject> {
        self.upstream.box_clone()
    }

    fn zone_type(&self) -> ZoneType {
        self.upstream.zone_type()
    }

    fn is_axfr_allowed(&self) -> bool {
        self.upstream.is_axfr_allowed()
    }

    async fn update(&self, update: &MessageRequest) -> UpdateResult<bool> {
        self.upstream.update(update).await
    }

    fn origin(&self) -> &LowerName {
        self.upstream.origin()
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        lookup_options: LookupOptions,
    ) -> Result<Box<dyn LookupObject>, LookupError> {
        self.upstream.lookup(name, rtype, lookup_options).await
    }

    async fn search(
        &self,
        request_info: RequestInfo<'_>,
        lookup_options: LookupOptions,
    ) -> Result<Box<dyn LookupObject>, LookupError> {
        let future = self.handler.handle(request_info, lookup_options);
        // let future = self.upstream.search(request_info, lookup_options);

        match time::timeout(Duration::from_millis(2000), future).await {
            Ok(r) => r,
            Err(_) => Err(LookupError::ResponseCode(ResponseCode::ServFail)),
        }
    }

    async fn get_nsec_records(
        &self,
        name: &LowerName,
        lookup_options: LookupOptions,
    ) -> Result<Box<dyn LookupObject>, LookupError> {
        self.upstream.get_nsec_records(name, lookup_options).await
    }
}

pub struct Handler {
    catalog: Catalog,
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    async fn handle_request<R: ResponseHandler>(
        &self,
        request: &Request,
        response_handle: R,
    ) -> ResponseInfo {
        self.catalog.lookup(request, None, response_handle).await
    }
}
