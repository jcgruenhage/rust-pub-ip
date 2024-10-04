use std::borrow::Cow;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::str;
use std::task::{Context, Poll};

use futures_core::Stream;
use futures_util::future::BoxFuture;
use futures_util::{future, ready, stream};
// use hyper::{
//     body::{self, Body, Buf},
//     client::Builder,
// };
use pin_project_lite::pin_project;
use reqwest::Url;
use thiserror::Error;
use tracing::trace_span;
use tracing_futures::Instrument;

// use hyper::client::connect::{HttpConnector, HttpInfo};

// type GaiResolver = hyper_system_resolver::system::Resolver;

use crate::{Resolutions, Version};

///////////////////////////////////////////////////////////////////////////////
// Hardcoded resolvers

/// All builtin HTTP/HTTPS resolvers.
pub const ALL: &dyn crate::Resolver<'static> = &&[HTTP, HTTPS];

/// All builtin HTTP resolvers.
pub const HTTP: &dyn crate::Resolver<'static> = &&[
    #[cfg(feature = "ipify-org")]
    HTTP_IPIFY_ORG,
];

/// `http://api.ipify.org` HTTP resolver options
#[cfg(feature = "ipify-org")]
#[cfg_attr(docsrs, doc(cfg(feature = "ipify-org")))]
pub const HTTP_IPIFY_ORG: &dyn crate::Resolver<'static> =
    &Resolver::new_static("http://api.ipify.org", ExtractMethod::PlainText);

/// All builtin HTTP resolvers.
pub const HTTPS: &dyn crate::Resolver<'static> = &&[
    #[cfg(feature = "ipify-org")]
    HTTPS_IPIFY_ORG,
    #[cfg(feature = "myip-com")]
    HTTPS_MYIP_COM,
    #[cfg(feature = "my-ip-io")]
    HTTPS_MY_IP_IO,
    #[cfg(feature = "seeip-org")]
    HTTPS_SEEIP_ORG,
];

/// `http://api.ipify.org` HTTP resolver options
#[cfg(feature = "ipify-org")]
#[cfg_attr(docsrs, doc(cfg(feature = "ipify-org")))]
pub const HTTPS_IPIFY_ORG: &dyn crate::Resolver<'static> =
    &Resolver::new_static("https://api.ipify.org", ExtractMethod::PlainText);

/// `https://api.myip.com` HTTPS resolver options
#[cfg(feature = "myip-com")]
#[cfg_attr(docsrs, doc(cfg(feature = "myip-com")))]
pub const HTTPS_MYIP_COM: &dyn crate::Resolver<'static> =
    &Resolver::new_static("https://api.myip.com", ExtractMethod::ExtractJsonIpField);

/// `https://api.my-ip.io/ip` HTTPS resolver options
#[cfg(feature = "my-ip-io")]
#[cfg_attr(docsrs, doc(cfg(feature = "my-ip-io")))]
pub const HTTPS_MY_IP_IO: &dyn crate::Resolver<'static> =
    &Resolver::new_static("https://api.my-ip.io/ip", ExtractMethod::PlainText);

/// `https://ip.seeip.org` HTTPS resolver options
#[cfg(feature = "seeip-org")]
#[cfg_attr(docsrs, doc(cfg(feature = "seeip-org")))]
pub const HTTPS_SEEIP_ORG: &dyn crate::Resolver<'static> =
    &Resolver::new_static("https://ip.seeip.org", ExtractMethod::PlainText);

///////////////////////////////////////////////////////////////////////////////
// Error

/// HTTP resolver error
#[derive(Debug, Error)]
pub enum Error {
    /// Client error.
    #[error("{0}")]
    Client(#[from] reqwest::Error),
    /// URL parsing error.
    #[error("{0}")]
    Url(#[from] url::ParseError),
}

///////////////////////////////////////////////////////////////////////////////
// Details & options

/// A resolution produced from a HTTP resolver
#[derive(Debug, Clone)]
pub struct Details {
    url: Url,
    server: Option<SocketAddr>,
    method: ExtractMethod,
}

impl Details {
    /// URL used in the resolution of the associated IP address
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// HTTP server used in the resolution of our IP address.
    pub fn server(&self) -> Option<SocketAddr> {
        self.server
    }

    /// The extract method used in the resolution of the associated IP address
    pub fn extract_method(&self) -> ExtractMethod {
        self.method
    }
}

/// Method used to extract an IP address from a http response
#[derive(Debug, Clone, Copy)]
pub enum ExtractMethod {
    /// Parses the body with whitespace trimmed as the IP address.
    PlainText,
    /// Parses the body with double quotes and whitespace trimmed as the IP address.
    StripDoubleQuotes,
    /// Parses the value of the JSON property `"ip"` within the body as the IP address.
    ///
    /// Note this method does not validate the JSON.
    ExtractJsonIpField,
}

///////////////////////////////////////////////////////////////////////////////
// Resolver

/// Options to build a HTTP resolver
#[derive(Debug, Clone)]
pub struct Resolver<'r> {
    url: Cow<'r, str>,
    method: ExtractMethod,
}

impl<'r> Resolver<'r> {
    /// Create new HTTP resolver options
    pub fn new<U>(url: U, method: ExtractMethod) -> Self
    where
        U: Into<Cow<'r, str>>,
    {
        Self {
            url: url.into(),
            method,
        }
    }
}

impl Resolver<'static> {
    /// Create new HTTP resolver options from static
    #[must_use]
    pub const fn new_static(url: &'static str, method: ExtractMethod) -> Self {
        Self {
            url: Cow::Borrowed(url),
            method,
        }
    }
}

///////////////////////////////////////////////////////////////////////////////
// Resolutions

pin_project! {
    #[project = HttpResolutionsProj]
    enum HttpResolutions<'r> {
        HttpRequest {
            #[pin]
            response: BoxFuture<'r, Result<(IpAddr, crate::Details), crate::Error>>,
        },
        Done,
    }
}

impl<'r> Stream for HttpResolutions<'r> {
    type Item = Result<(IpAddr, crate::Details), crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().project() {
            HttpResolutionsProj::HttpRequest { response } => {
                let response = ready!(response.poll(cx));
                *self = HttpResolutions::Done;
                Poll::Ready(Some(response))
            }
            HttpResolutionsProj::Done => Poll::Ready(None),
        }
    }
}

#[derive(serde::Deserialize)]
struct JsonIp {
    ip: String,
}

async fn resolve(
    version: Version,
    url: Url,
    method: ExtractMethod,
) -> Result<(IpAddr, crate::Details), crate::Error> {
    let mut client_builder = reqwest::Client::builder();
    client_builder = match version {
        Version::V4 => client_builder.local_address(Some("0.0.0.0".parse()?)),
        Version::V6 => client_builder.local_address(Some("[::]:0".parse()?)),
        Version::Any => client_builder,
    };
    let client = client_builder.build()?;
    let response = client.get(url.clone()).send().await?;
    // TODO
    let server = response.remote_addr();
    let address_str = match method {
        ExtractMethod::PlainText => response.text().await?.trim().to_owned(),
        ExtractMethod::ExtractJsonIpField => response.json::<JsonIp>().await?.ip,
        ExtractMethod::StripDoubleQuotes => {
            response.text().await?.trim().trim_matches('"').to_owned()
        }
    };
    let address = address_str.parse()?;
    let details = Box::new(Details {
        url,
        server,
        method,
    });
    Ok((address, crate::Details::from(details)))
}

impl<'r> crate::Resolver<'r> for Resolver<'r> {
    fn resolve(&self, version: Version) -> Resolutions<'r> {
        let method = self.method;
        let url: Url = match self.url.as_ref().parse() {
            Ok(name) => name,
            Err(err) => return Box::pin(stream::once(future::ready(Err(crate::Error::new(err))))),
        };
        let span = trace_span!("http resolver", ?version, ?method, %url);
        let resolutions = HttpResolutions::HttpRequest {
            response: Box::pin(resolve(version, url, method)),
        };
        Box::pin(resolutions.instrument(span))
    }
}
