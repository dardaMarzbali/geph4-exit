use std::{
    net::{Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
    time::Duration,
};

use crate::{config::CONFIG, ratelimit::RateLimiter, root_ctx::ROOT_CTX};
use anyhow::Context;
use cidr_utils::cidr::Ipv6Cidr;

use moka::sync::Cache;
use once_cell::sync::Lazy;
use smol::prelude::*;
use smol::{
    io::{AsyncRead, AsyncWrite},
    Async,
};
use smol_timeout::TimeoutExt;
use socket2::{Domain, Protocol, Socket, Type};
use tap::TapFallible;

async fn resolve_name_inner(name: String) -> anyhow::Result<SocketAddr> {
    static DNS_CACHE: Lazy<Cache<String, SocketAddr>> = Lazy::new(|| {
        Cache::builder()
            .max_capacity(1_000_000)
            .time_to_live(Duration::from_secs(3600))
            .build()
    });

    if let Some(v) = DNS_CACHE.get(&name) {
        return Ok(v);
    }

    let vec: Vec<SocketAddr> = smol::net::resolve(&name).await?.into_iter().collect();

    if let Some(s) = vec.get(0) {
        DNS_CACHE.insert(name, *s);
        Ok(*s)
    } else {
        anyhow::bail!("no suitable IP address")
    }
}

async fn resolve_name(name: String) -> anyhow::Result<SocketAddr> {
    for _ in 0..3 {
        if let Ok(a) = resolve_name_inner(name.clone()).await {
            return Ok(a);
        }
    }
    resolve_name_inner(name.clone()).await
}

/// Connects to a remote host and forwards traffic to/from it and a given client.
pub async fn proxy_loop(
    rate_limit: Arc<RateLimiter>,
    client: impl AsyncRead + AsyncWrite + Clone + Unpin + Send + 'static,
    client_id: u64,
    addr: String,
    _count_stats: bool,
) -> anyhow::Result<()> {
    let f = async move {
        // Incr/decr the connection count
        ROOT_CTX
            .conn_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _deferred = scopeguard::guard((), |_| {
            ROOT_CTX
                .conn_count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });

        // First, we establish a TCP connection
        let addr = resolve_name(addr.clone())
            .await
            .tap_err(|err| log::warn!("cannot resolve remote {}: {}", addr, err))?;

        // Reject if blacklisted
        if crate::lists::BLACK_PORTS.contains(&addr.port()) {
            anyhow::bail!("port blacklisted")
        }
        if CONFIG.port_whitelist() && !crate::lists::WHITE_PORTS.contains(&addr.port()) {
            anyhow::bail!("port {} not whitelisted", addr.port())
        }

        // Obtain ASN
        log::debug!(
            "got connection request to {}  (conn_count = {})",
            CONFIG.redact(addr),
            ROOT_CTX
                .conn_count
                .load(std::sync::atomic::Ordering::Relaxed)
        );

        // Upload official stats
        let upload_stat = Arc::new(move |n| ROOT_CTX.incr_throughput(n));

        let remote = if let Some(pool) =
            CONFIG
                .random_ipv6_range()
                .and_then(|a| if addr.is_ipv6() { Some(a) } else { None })
        {
            let pool: Ipv6Cidr = pool;
            fastrand::seed(client_id);
            let random_ipv6 = Ipv6Addr::from(fastrand::u128(pool.first()..=pool.last()));
            log::trace!("assigned {:?}", random_ipv6);
            let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_nonblocking(true)?;
            socket.set_reuse_address(true)?;
            socket.set_reuse_port(true)?;
            let sock_addr = SocketAddrV6::new(random_ipv6, 0, 0, 0);
            socket.bind(&sock_addr.into()).context("can't bind")?;
            let _ = socket.connect(&addr.into()); // this is gonna return einprogress and it's fine
            let stream =
                Async::new(std::net::TcpStream::from(socket)).context("can't make Async")?;
            stream.writable().await?;
            stream
        } else {
            Async::<std::net::TcpStream>::connect(addr)
                .timeout(Duration::from_secs(60))
                .await
                .ok_or_else(|| anyhow::anyhow!("connect timed out for {}", addr))??
        };
        remote.as_ref().set_nodelay(true)?;

        let remote = async_dup::Arc::new(remote);
        let remote2 = remote.clone();
        let client2 = client.clone();
        // let _t = smolscale::spawn(async move {
        //     let _ = smol::io::copy(remote2, client2).await;
        // });
        // smol::io::copy(client, remote).await?;

        let us1 = upload_stat.clone();
        let _up = smolscale::spawn(geph4_aioutils::copy_with_stats_async(
            remote2,
            client2,
            move |n| {
                us1(n);
                let rate_limit = rate_limit.clone();
                async move {
                    rate_limit.wait(n).await;
                }
            },
        ));
        geph4_aioutils::copy_with_stats(client, remote, move |n| {
            upload_stat(n);
        })
        .or(async {
            // "grace period"
            smol::Timer::after(Duration::from_secs(30)).await;
            let killer = ROOT_CTX.kill_event.listen();
            killer.await;
            log::warn!("killing connection due to connection kill event");
            Ok(())
        })
        .await?;

        Ok(())
        // let down = smolscale::spawn();

        // smol::future::race(up, down)
        //     .or(async {
        //         // "grace period"
        //         smol::Timer::after(Duration::from_secs(30)).await;
        //         let killer = ROOT_CTX.kill_event.listen();
        //         killer.await;
        //         log::warn!("killing connection due to connection kill event");
        //         Ok(())
        //     })
        //     .await?;
        // anyhow::Ok(())
    };
    if let Err(err) = f.await {
        log::trace!("conn failed w/ {:?}", err);
    }
    Ok(())
}
