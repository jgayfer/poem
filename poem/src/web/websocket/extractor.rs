use std::{borrow::Cow, future::Future};

use headers::HeaderMapExt;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::{utils::sign, WebSocketStream};
use crate::{
    error::WebSocketError,
    http::{
        header::{self, HeaderValue},
        Method, StatusCode,
    },
    Body, FromRequest, IntoResponse, Request, RequestBody, Response, Result,
};

/// An extractor that can accept websocket connections.
///
/// # Errors
///
/// - [`WebSocketError`]
pub struct WebSocket {
    key: HeaderValue,
    #[cfg(not(target_os = "wasi"))]
    on_upgrade: hyper::OnUpgrade,
    #[cfg(target_os = "wasi")]
    request_body: Body,
    protocols: Option<Box<[Cow<'static, str>]>>,
    sec_websocket_protocol: Option<HeaderValue>,
}

#[async_trait::async_trait]
impl<'a> FromRequest<'a> for WebSocket {
    #[allow(unused_variables)]
    async fn from_request(req: &'a Request, body: &mut RequestBody) -> Result<Self> {
        if req.method() != Method::GET
            || req.headers().get(header::UPGRADE) != Some(&HeaderValue::from_static("websocket"))
            || req.headers().get(header::SEC_WEBSOCKET_VERSION)
                != Some(&HeaderValue::from_static("13"))
        {
            return Err(WebSocketError::InvalidProtocol.into());
        }

        if !matches!(
            req.headers()
                .typed_get::<headers::Connection>()
                .map(|connection| connection.contains(header::UPGRADE)),
            Some(true)
        ) {
            return Err(WebSocketError::InvalidProtocol.into());
        }

        let key = req
            .headers()
            .get(header::SEC_WEBSOCKET_KEY)
            .cloned()
            .ok_or(WebSocketError::InvalidProtocol)?;

        let sec_websocket_protocol = req.headers().get(header::SEC_WEBSOCKET_PROTOCOL).cloned();

        Ok(Self {
            key,
            #[cfg(not(target_os = "wasi"))]
            on_upgrade: req.take_upgrade()?,
            #[cfg(target_os = "wasi")]
            request_body: body.take()?,
            protocols: None,
            sec_websocket_protocol,
        })
    }
}

impl WebSocket {
    /// Set the known protocols.
    ///
    /// If the protocol name specified by `Sec-WebSocket-Protocol` header
    /// to match any of them, the upgrade response will include
    /// `Sec-WebSocket-Protocol` header and return the protocol name.
    ///
    /// ```
    /// use futures_util::{SinkExt, StreamExt};
    /// use poem::{get, handler, web::websocket::WebSocket, IntoResponse, Route};
    ///
    /// #[handler]
    /// async fn index(ws: WebSocket) -> impl IntoResponse {
    ///     ws.protocols(vec!["graphql-rs", "graphql-transport-ws"])
    ///         .on_upgrade(|socket| async move {
    ///             // ...
    ///         })
    /// }
    ///
    /// let app = Route::new().at("/", get(index));
    /// ```
    #[must_use]
    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cow<'static, str>>,
    {
        self.protocols = Some(
            protocols
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into(),
        );
        self
    }

    /// Finalize upgrading the connection and call the provided `callback` with
    /// the stream.
    ///
    /// Note that the return value of this function must be returned from the
    /// handler.
    #[must_use]
    pub fn on_upgrade<F, Fut>(self, callback: F) -> impl IntoResponse
    where
        F: FnOnce(WebSocketStream) -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        WebSocketUpgraded {
            websocket: self,
            callback,
        }
    }
}

struct WebSocketUpgraded<F> {
    websocket: WebSocket,
    callback: F,
}

impl<F, Fut> IntoResponse for WebSocketUpgraded<F>
where
    F: FnOnce(WebSocketStream) -> Fut + Send + Sync + 'static,
    Fut: Future + Send + 'static,
{
    fn into_response(self) -> Response {
        // check requested protocols
        let protocol = self
            .websocket
            .sec_websocket_protocol
            .as_ref()
            .and_then(|req_protocols| {
                let req_protocols = req_protocols.to_str().ok()?;
                let protocols = self.websocket.protocols.as_ref()?;
                req_protocols
                    .split(',')
                    .map(|req_p| req_p.trim())
                    .find(|req_p| protocols.iter().any(|p| p == req_p))
            });

        let mut builder = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(
                header::SEC_WEBSOCKET_ACCEPT,
                sign(self.websocket.key.as_bytes()),
            );

        if let Some(protocol) = protocol {
            builder = builder.header(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_str(protocol).unwrap(),
            );
        }

        let resp = builder.body(Body::empty());

        crate::runtime::spawn(async move {
            #[cfg(not(target_os = "wasi"))]
            let stream = match self.websocket.on_upgrade.await {
                Ok(stream) => stream,
                Err(_) => return,
            };

            #[cfg(target_os = "wasi")]
            let stream = {
                let request_body_reader = self.websocket.request_body.into_async_read();
                let response_body_writer = crate::runtime::wasi::ResponseWriter;
                super::unite_stream::UniteStream::<
                    Box<dyn tokio::io::AsyncRead + Send + Unpin>,
                    Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
                >::new(
                    Box::new(request_body_reader),
                    Box::new(response_body_writer),
                )
            };

            let stream =
                tokio_tungstenite::WebSocketStream::from_raw_socket(stream, Role::Server, None)
                    .await;
            (self.callback)(WebSocketStream::new(stream)).await;
        });

        resp
    }
}
