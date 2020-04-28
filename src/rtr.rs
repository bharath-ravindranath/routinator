/// Support for the RPKI-to-Router Protocol.

use std::io;
use std::future::Future;
use std::net::TcpListener as StdListener;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use futures::pin_mut;
use futures::future::{pending, select_all};
use tokio::stream::Stream;
use log::error;
use rpki_rtr::server::{NotifySender, Server};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use crate::config::Config;
use crate::metrics::ServerMetrics;
use crate::operation::ExitError;
use crate::origins::OriginsHistory;
use std::time::Duration;


pub fn rtr_listener(
    history: OriginsHistory,
    config: &Config
) -> Result<(NotifySender, impl Future<Output = ()>), ExitError> {
    let sender = NotifySender::new();

    let mut listeners = Vec::new();
    for addr in &config.rtr_listen {
        // Binding needs to have happened before dropping privileges
        // during detach. So we do this here synchronously.
        match StdListener::bind(addr) {
            Ok(listener) => listeners.push(listener),
            Err(err) => {
                error!("Fatal error listening on {}: {}", addr, err);
                return Err(ExitError::Generic);
            }
        };
    }
    Ok((sender.clone(), _rtr_listener(history, sender, listeners)))
}

async fn _rtr_listener(
    origins: OriginsHistory,
    sender: NotifySender,
    listeners: Vec<StdListener>,
) {
    if listeners.is_empty() {
        pending::<()>().await;
    }
    else {
        let _ = select_all(
            listeners.into_iter().map(|listener| {
                tokio::spawn(single_rtr_listener(
                    listener, origins.clone(), sender.clone()
                ))
            })
        ).await;
    }
}

async fn single_rtr_listener(
    listener: StdListener,
    origins: OriginsHistory,
    sender: NotifySender,
) {
    let listener = RtrListener {
        sock: match TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                error!("Fatal error on RTR listener: {}", err);
                return;
            }
        },
        metrics: origins.server_metrics()
    };
    if Server::new(listener, sender, origins.clone()).run().await.is_err() {
        error!("Fatal error listening for RTR connections.");
    }
}


struct RtrListener {
    sock: TcpListener,
    metrics: Arc<ServerMetrics>,
}

impl Stream for RtrListener {
    type Item = Result<RtrStream, io::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>, cx: &mut Context
    ) -> Poll<Option<Self::Item>> {
        let sock = &mut self.sock;
        pin_mut!(sock);
        sock.poll_next(cx).map(|sock| sock.map(|sock| sock.map(|sock| {
            sock.set_keepalive(Some(Duration::new(60, 0))).expect("set_keepalive failed");
            self.metrics.inc_rtr_conn_open();
            RtrStream {
                sock,
                metrics: self.metrics.clone()
            }
        })))
    }
}

struct RtrStream {
    sock: TcpStream,
    metrics: Arc<ServerMetrics>,
}

impl AsyncRead for RtrStream {
    fn poll_read(
        mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]
    ) -> Poll<Result<usize, io::Error>> {
        let sock = &mut self.sock;
        pin_mut!(sock);
        let res = sock.poll_read(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            self.metrics.inc_rtr_bytes_read(n as u64)
        }
        res
    }
}

impl AsyncWrite for RtrStream {
    fn poll_write(
        mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]
    ) -> Poll<Result<usize, io::Error>> {
        let sock = &mut self.sock;
        pin_mut!(sock);
        let res = sock.poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            self.metrics.inc_rtr_bytes_written(n as u64)
        }
        res
    }

    fn poll_flush(
        mut self: Pin<&mut Self>, cx: &mut Context
    ) -> Poll<Result<(), io::Error>> {
        let sock = &mut self.sock;
        pin_mut!(sock);
        sock.poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>, cx: &mut Context
    ) -> Poll<Result<(), io::Error>> {
        let sock = &mut self.sock;
        pin_mut!(sock);
        sock.poll_shutdown(cx)
    }
}

impl Drop for RtrStream {
    fn drop(&mut self) {
        self.metrics.inc_rtr_conn_close()
    }
}

