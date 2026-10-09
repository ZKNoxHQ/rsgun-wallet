//! The light node: one libp2p swarm task, plus tasks for inbound streams and filter upkeep.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, Transport,
    core::{
        muxing::StreamMuxerBox,
        transport::Boxed,
        upgrade::{SelectUpgrade, Version as UpgradeVersion},
    },
    identify,
    identity::Keypair,
    multiaddr::Protocol,
    noise, ping,
    swarm::{
        ConnectionId, DialError, NetworkBehaviour, SwarmEvent,
        dial_opts::{DialOpts, PeerCondition},
    },
    yamux,
};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::{
    Config, Error, Message, PublishReport, Status, filter,
    rt::{self, Instant, Interval, SystemTime, Task, UNIX_EPOCH},
    hash::{SeenSet, message_hash},
    lightpush::{self, PushError, Version},
    metadata,
    proto::{FilterSubscribeRequest, WakuMessage, filter_subscribe_type as fst},
    protocols::{FILTER_PUSH, FILTER_SUBSCRIBE, LIGHTPUSH_V2, LIGHTPUSH_V3, METADATA},
};

/// Delay before retrying a filter request refused by a peer.
const FILTER_RETRY: Duration = Duration::from_secs(10);
const UPKEEP_TICK: Duration = Duration::from_secs(5);

#[derive(NetworkBehaviour)]
struct Behaviour {
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    stream: libp2p_stream::Behaviour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Cluster {
    #[default]
    Unknown,
    Checking,
    Ok,
    Mismatch(u32),
}

struct Subscription {
    topics: BTreeSet<String>,
    last_ok: Instant,
}

#[derive(Default)]
struct Peer {
    connected: bool,
    /// Announced by identify; `None` until it arrives.
    protocols: Option<HashSet<String>>,
    cluster: Cluster,
    filter: Option<Subscription>,
    filter_busy: bool,
    filter_retry_at: Option<Instant>,
}

impl Peer {
    fn speaks(&self, protocol: &str) -> bool {
        self.protocols.as_ref().is_some_and(|p| p.contains(protocol))
    }

    fn usable(&self) -> bool {
        self.connected && !matches!(self.cluster, Cluster::Mismatch(_))
    }
}

struct State {
    peers: HashMap<PeerId, Peer>,
    topics: BTreeSet<String>,
    inbox: VecDeque<Message>,
    seen: SeenSet,
    last_error: Option<String>,
}

struct Inner {
    config: Config,
    state: Mutex<State>,
    wake: Notify,
    next_id: AtomicU64,
    id_prefix: String,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn request_id(&self) -> String {
        format!("{}-{}", self.id_prefix, self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn error(&self, e: impl Into<String>) {
        let e = e.into();
        debug!(error = %e, "waku");
        self.state().last_error = Some(e);
    }
}

/// A running light node. Dropping it stops every task and closes the connections.
pub struct LightNode {
    inner: Arc<Inner>,
    control: libp2p_stream::Control,
    local_peer_id: PeerId,
    tasks: Vec<Task>,
}

impl Drop for LightNode {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// A bootstrap entry: where to dial, and which identity to require there.
struct Bootstrap {
    /// Without the `/p2p/` suffix.
    addr: Multiaddr,
    pinned: Option<PeerId>,
    /// `/wss` (or `/tls/ws`): the host is authenticated by its certificate.
    wss: bool,
    /// Identity of the live connection, once established.
    peer: Option<PeerId>,
    next_dial: Instant,
    delay: Duration,
}

fn is_wss(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::Wss(_) | Protocol::Tls))
}

fn split_bootstrap(addrs: &[Multiaddr], base_delay: Duration) -> Result<Vec<Bootstrap>, Error> {
    let now = Instant::now();
    addrs
        .iter()
        .map(|a| {
            let mut addr = a.clone();
            let pinned = match addr.iter().last() {
                Some(Protocol::P2p(peer)) => {
                    addr.pop();
                    Some(peer)
                }
                _ => None,
            };
            let wss = is_wss(&addr);
            if pinned.is_none() && !wss {
                // Nothing would authenticate the remote at all.
                return Err(Error::Config(format!("{a}: missing /p2p/<peer id> on a non-wss address")));
            }
            Ok(Bootstrap { addr, pinned, wss, peer: None, next_dial: now, delay: base_delay })
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
type BaseTransport = libp2p::websocket::Config<libp2p::dns::tokio::Transport<libp2p::tcp::tokio::Transport>>;
#[cfg(target_arch = "wasm32")]
type BaseTransport = libp2p::websocket_websys::Transport;

/// Native: libp2p's WebSocket transport over DNS + TCP, TLS by rustls.
#[cfg(not(target_arch = "wasm32"))]
fn base_transport() -> BaseTransport {
    use libp2p::{dns, tcp, websocket};
    let tcp = || tcp::tokio::Transport::new(tcp::Config::default().nodelay(true));
    // Android and some sandboxes have no resolv.conf: fall back to public resolvers.
    let dns = dns::tokio::Transport::system(tcp()).unwrap_or_else(|e| {
        warn!(error = %e, "no system DNS configuration, using Cloudflare resolvers");
        dns::tokio::Transport::custom(
            tcp(),
            hickory_resolver::config::ResolverConfig::udp_and_tcp(
                &hickory_resolver::config::CLOUDFLARE,
            ),
            dns::ResolverOpts::default(),
        )
    });
    websocket::Config::new(dns)
}

/// Browser: the page's (or worker's) own WebSocket; DNS and TLS are the browser's.
#[cfg(target_arch = "wasm32")]
fn base_transport() -> BaseTransport {
    libp2p::websocket_websys::Transport::default()
}

fn build_transport(key: &Keypair, dial_timeout: Duration) -> Result<Boxed<(PeerId, StreamMuxerBox)>, Error> {
    let noise = noise::Config::new(key).map_err(|e| Error::Setup(e.to_string()))?;
    Ok(base_transport()
        .upgrade(UpgradeVersion::V1Lazy)
        .authenticate(noise)
        .multiplex(SelectUpgrade::new(yamux::Config::default(), libp2p_mplex::Config::new()))
        .timeout(dial_timeout)
        .map(|(peer, muxer), _| (peer, StreamMuxerBox::new(muxer)))
        .boxed())
}

#[cfg(not(target_arch = "wasm32"))]
fn swarm_config() -> libp2p::swarm::Config {
    libp2p::swarm::Config::with_tokio_executor()
}

#[cfg(target_arch = "wasm32")]
fn swarm_config() -> libp2p::swarm::Config {
    libp2p::swarm::Config::with_wasm_executor()
}

impl LightNode {
    /// Starts the node on the current tokio runtime and dials the bootstrap peers. Returns at
    /// once; watch [`LightNode::status`] or [`LightNode::wait_subscribed`].
    pub fn start(config: Config) -> Result<Self, Error> {
        if config.bootstrap.is_empty() {
            return Err(Error::Config("no bootstrap peer".into()));
        }
        let bootstrap = split_bootstrap(&config.bootstrap, config.redial_delay)?;
        let key = Keypair::generate_ed25519();
        let local_peer_id = key.public().to_peer_id();

        let transport = build_transport(&key, config.dial_timeout)?;
        let behaviour = Behaviour {
            identify: identify::Behaviour::new(
                identify::Config::new("/ipfs/id/1.0.0".into(), key.public())
                    .with_agent_version(config.agent_version.clone()),
            ),
            ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(20))),
            stream: libp2p_stream::Behaviour::new(),
        };
        let mut control = behaviour.stream.new_control();
        let swarm = Swarm::new(
            transport,
            behaviour,
            local_peer_id,
            swarm_config().with_idle_connection_timeout(Duration::from_secs(365 * 24 * 3600)),
        );

        let pushes = control
            .accept(StreamProtocol::new(FILTER_PUSH))
            .map_err(|e| Error::Setup(e.to_string()))?;
        let metadata_queries = control
            .accept(metadata::PROTOCOL)
            .map_err(|e| Error::Setup(e.to_string()))?;

        let id = local_peer_id.to_base58();
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                peers: HashMap::new(),
                topics: BTreeSet::new(),
                inbox: VecDeque::new(),
                seen: SeenSet::new(config.inbox_capacity.max(1024) * 4),
                last_error: None,
            }),
            wake: Notify::new(),
            next_id: AtomicU64::new(1),
            id_prefix: id[id.len().saturating_sub(8)..].to_string(),
            config,
        });
        info!(peer = %local_peer_id, "waku light node starting");

        let tasks = vec![
            rt::spawn(run_swarm(swarm, inner.clone(), bootstrap)),
            rt::spawn(run_pushes(inner.clone(), pushes)),
            rt::spawn(run_metadata(inner.clone(), metadata_queries)),
            rt::spawn(run_upkeep(inner.clone(), control.clone())),
        ];
        Ok(Self { inner, control, local_peer_id, tasks })
    }

    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub fn pubsub_topic(&self) -> &str {
        &self.inner.config.pubsub_topic
    }

    /// Adds content topics to the filter subscription (on every service peer). Idempotent.
    pub fn subscribe<I: IntoIterator<Item = S>, S: Into<String>>(&self, topics: I) {
        self.inner.state().topics.extend(topics.into_iter().map(Into::into));
        self.inner.wake.notify_one();
    }

    pub fn unsubscribe<I: IntoIterator<Item = S>, S: Into<String>>(&self, topics: I) {
        {
            let mut st = self.inner.state();
            for t in topics {
                st.topics.remove(&t.into());
            }
        }
        self.inner.wake.notify_one();
    }

    pub fn topics(&self) -> Vec<String> {
        self.inner.state().topics.iter().cloned().collect()
    }

    /// Messages received since the previous call, deduplicated, oldest first.
    pub fn drain(&self) -> Vec<Message> {
        self.inner.state().inbox.drain(..).collect()
    }

    pub fn status(&self) -> Status {
        let st = self.inner.state();
        let usable = st.peers.values().filter(|p| p.usable());
        Status {
            connected_peers: st.peers.values().filter(|p| p.connected).count(),
            service_peers: usable
                .filter(|p| {
                    p.speaks(FILTER_SUBSCRIBE) && (p.speaks(LIGHTPUSH_V3) || p.speaks(LIGHTPUSH_V2))
                })
                .count(),
            filter_subscriptions: st.peers.values().filter(|p| p.usable() && p.filter.is_some()).count(),
            last_error: st.last_error.clone(),
        }
    }

    /// Waits until at least one service node holds the full current topic set.
    pub async fn wait_subscribed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let st = self.inner.state();
                let topics = st.topics.clone();
                if st.peers.values().any(|p| {
                    p.usable() && p.filter.as_ref().is_some_and(|s| s.topics == topics)
                }) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            rt::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Light push to up to `max_push_peers` service nodes, concurrently. `Ok` when at least one
    /// accepted.
    pub async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<PublishReport, Error> {
        let targets: Vec<(PeerId, Version)> = {
            let st = self.inner.state();
            st.peers
                .iter()
                .filter(|(_, p)| p.usable())
                .filter_map(|(id, p)| {
                    if p.speaks(LIGHTPUSH_V3) {
                        Some((*id, Version::V3))
                    } else if p.speaks(LIGHTPUSH_V2) {
                        Some((*id, Version::V2))
                    } else {
                        None
                    }
                })
                .take(self.inner.config.max_push_peers.max(1))
                .collect()
        };
        if targets.is_empty() {
            return Err(Error::NoPeer("light push"));
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .ok();
        let message = WakuMessage {
            payload: payload.to_vec(),
            content_topic: content_topic.to_string(),
            version: Some(0),
            timestamp,
            ..Default::default()
        };
        let pubsub = self.inner.config.pubsub_topic.clone();
        let timeout = self.inner.config.request_timeout;

        let outcomes = futures::future::join_all(targets.into_iter().map(|(peer, version)| {
            let mut control = self.control.clone();
            let message = message.clone();
            let pubsub = pubsub.clone();
            let inner = self.inner.clone();
            async move {
                let mut used = version;
                let mut result = lightpush::push(
                    &mut control, peer, version, inner.request_id(), &pubsub, message.clone(), timeout,
                )
                .await;
                if matches!(result, Err(PushError::Unsupported)) && version == Version::V3 {
                    used = Version::V2;
                    result = lightpush::push(
                        &mut control, peer, Version::V2, inner.request_id(), &pubsub, message, timeout,
                    )
                    .await;
                }
                (peer, used, result)
            }
        }))
        .await;

        let mut report = PublishReport::default();
        for (peer, used, result) in outcomes {
            match result {
                Ok(()) => {
                    report.accepted += 1;
                    let v = if used == Version::V3 { "v3" } else { "v2" };
                    report.accepted_via.push(format!("{peer} (lightpush {v})"));
                }
                Err(PushError::Unsupported) => report.failures.push(format!("{peer}: light push not supported")),
                Err(PushError::Failed(e)) => report.failures.push(format!("{peer}: {e}")),
            }
        }
        if report.accepted == 0 {
            let e = Error::Refused(report.failures);
            self.inner.error(e.to_string());
            return Err(e);
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------------------------

async fn run_swarm(mut swarm: Swarm<Behaviour>, inner: Arc<Inner>, mut boots: Vec<Bootstrap>) {
    let base = inner.config.redial_delay;
    let max = inner.config.max_redial_delay;
    let unpin_wss = inner.config.accept_rotated_wss_identity;
    // Pending dial -> bootstrap entry.
    let mut dialing: HashMap<ConnectionId, usize> = HashMap::new();
    let mut tick = Interval::new(Duration::from_secs(1));

    fn backoff(b: &mut Bootstrap, max: Duration) {
        b.next_dial = Instant::now() + b.delay;
        b.delay = (b.delay * 2).min(max);
    }

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = Instant::now();
                for (i, b) in boots.iter_mut().enumerate() {
                    let connected = b.peer.is_some_and(|p| swarm.is_connected(&p));
                    if connected || b.next_dial > now || dialing.values().any(|j| *j == i) {
                        continue;
                    }
                    // Pushed back now; a failure event backs off further.
                    b.next_dial = now + max;
                    let opts = match b.pinned {
                        Some(peer) => DialOpts::peer_id(peer)
                            .addresses(vec![b.addr.clone()])
                            .condition(PeerCondition::DisconnectedAndNotDialing)
                            .build(),
                        None => DialOpts::unknown_peer_id().address(b.addr.clone()).build(),
                    };
                    let id = opts.connection_id();
                    match swarm.dial(opts) {
                        Ok(()) => {
                            debug!(addr = %b.addr, pinned = ?b.pinned, "dialling");
                            dialing.insert(id, i);
                        }
                        Err(e) => debug!(addr = %b.addr, error = %e, "dial not started"),
                    }
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } => {
                    info!(peer = %peer_id, addr = %endpoint.get_remote_address(), "waku peer connected");
                    if let Some(i) = dialing.remove(&connection_id) {
                        boots[i].peer = Some(peer_id);
                        boots[i].delay = base;
                    }
                    inner.state().peers.entry(peer_id).or_default().connected = true;
                    inner.wake.notify_one();
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established: 0, cause, .. } => {
                    let why = cause.map(|c| c.to_string()).unwrap_or_else(|| "closed".into());
                    info!(peer = %peer_id, reason = %why, "waku peer disconnected");
                    {
                        let mut st = inner.state();
                        if let Some(p) = st.peers.get_mut(&peer_id) {
                            *p = Peer::default();
                        }
                        st.last_error = Some(format!("{peer_id} disconnected: {why}"));
                    }
                    for b in boots.iter_mut().filter(|b| b.peer == Some(peer_id)) {
                        b.peer = None;
                        backoff(b, max);
                    }
                    inner.wake.notify_one();
                }
                SwarmEvent::OutgoingConnectionError { connection_id, error, .. } => {
                    let Some(i) = dialing.remove(&connection_id) else { continue };
                    let b = &mut boots[i];
                    match &error {
                        DialError::WrongPeerId { obtained, .. } if b.wss && unpin_wss => {
                            // The certificate already authenticated the host; the pin is stale.
                            warn!(
                                addr = %b.addr,
                                expected = ?b.pinned,
                                obtained = %obtained,
                                "bootstrap peer presents another identity; wss host authenticated by TLS, dialling it without pin"
                            );
                            inner.error(format!("{}: identity changed to {obtained}, pin dropped", b.addr));
                            b.pinned = None;
                            b.next_dial = Instant::now();
                        }
                        _ => {
                            warn!(addr = %b.addr, error = %error, "waku dial failed");
                            inner.error(format!("dial {}: {error}", b.addr));
                            backoff(b, max);
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                    let protocols: HashSet<String> = info.protocols.iter().map(ToString::to_string).collect();
                    debug!(peer = %peer_id, agent = %info.agent_version, ?protocols, "identified");
                    let mut st = inner.state();
                    let p = st.peers.entry(peer_id).or_default();
                    p.protocols = Some(protocols);
                    drop(st);
                    inner.wake.notify_one();
                }
                SwarmEvent::Behaviour(BehaviourEvent::Ping(ping::Event { peer, result: Err(e), .. })) => {
                    debug!(%peer, error = %e, "ping failed");
                }
                _ => {}
            }
        }
    }
}

async fn run_pushes(inner: Arc<Inner>, mut incoming: libp2p_stream::IncomingStreams) {
    while let Some((peer, stream)) = incoming.next().await {
        let inner = inner.clone();
        rt::spawn(async move {
            let push = match filter::read_push(stream, inner.config.request_timeout).await {
                Ok(p) => p,
                Err(e) => return debug!(%peer, error = %e, "bad filter push"),
            };
            let Some(msg) = push.waku_message else { return };
            let pubsub = push.pubsub_topic.as_deref().unwrap_or(&inner.config.pubsub_topic);
            if pubsub != inner.config.pubsub_topic {
                return debug!(%peer, pubsub, "push on another pubsub topic");
            }
            let hash = message_hash(pubsub, &msg);
            let mut st = inner.state();
            if !st.topics.contains(&msg.content_topic) || !st.seen.insert(hash) {
                return;
            }
            st.inbox.push_back(Message {
                content_topic: msg.content_topic,
                payload: msg.payload,
                timestamp_ns: msg.timestamp.and_then(|t| u64::try_from(t).ok()).filter(|t| *t > 0),
            });
            while st.inbox.len() > inner.config.inbox_capacity {
                st.inbox.pop_front();
            }
        });
    }
}

async fn run_metadata(inner: Arc<Inner>, mut incoming: libp2p_stream::IncomingStreams) {
    while let Some((peer, stream)) = incoming.next().await {
        let inner = inner.clone();
        rt::spawn(async move {
            let cfg = &inner.config;
            match metadata::serve(stream, cfg.cluster_id, &cfg.shards, cfg.request_timeout).await {
                Ok(remote) => record_cluster(&inner, peer, remote),
                Err(e) => debug!(%peer, error = %e, "metadata query not served"),
            }
        });
    }
}

fn record_cluster(inner: &Inner, peer: PeerId, remote: Option<u32>) {
    let ours = inner.config.cluster_id;
    let verdict = match remote {
        Some(c) if c != ours => Cluster::Mismatch(c),
        _ => Cluster::Ok,
    };
    if let Cluster::Mismatch(c) = verdict {
        warn!(%peer, cluster = c, ours, "waku peer on another cluster, ignored");
    }
    if let Some(p) = inner.state().peers.get_mut(&peer) {
        p.cluster = verdict;
    }
    inner.wake.notify_one();
}

// ---------------------------------------------------------------------------------------------

enum Job {
    Metadata,
    Filter { kind: i32, topics: Vec<String> },
}

/// Keeps the metadata check done and one filter subscription per service peer, matching the
/// wanted topics, pinged before the service node forgets it.
async fn run_upkeep(inner: Arc<Inner>, control: libp2p_stream::Control) {
    let mut tick = Interval::new(UPKEEP_TICK);
    loop {
        tokio::select! {
            _ = inner.wake.notified() => {}
            _ = tick.tick() => {}
        }
        for (peer, job) in plan(&inner) {
            let inner = inner.clone();
            let mut control = control.clone();
            rt::spawn(async move {
                match job {
                    Job::Metadata => {
                        let cfg = &inner.config;
                        let r = metadata::query(&mut control, peer, cfg.cluster_id, &cfg.shards, cfg.request_timeout).await;
                        match r {
                            Ok(remote) => record_cluster(&inner, peer, remote),
                            Err(e) => {
                                // Not fatal: nwaku answers, but a peer may not.
                                debug!(%peer, error = %e, "metadata query failed");
                                record_cluster(&inner, peer, None);
                            }
                        }
                    }
                    Job::Filter { kind, topics } => run_filter_job(&inner, &mut control, peer, kind, topics).await,
                }
            });
        }
    }
}

fn plan(inner: &Inner) -> Vec<(PeerId, Job)> {
    let cfg = &inner.config;
    let now = Instant::now();
    let mut st = inner.state();
    let wanted = st.topics.clone();
    let mut active = st.peers.values().filter(|p| p.usable() && p.filter.is_some()).count();
    let mut jobs = Vec::new();

    for (id, p) in st.peers.iter_mut() {
        if !p.usable() || p.protocols.is_none() {
            continue;
        }
        match p.cluster {
            Cluster::Unknown if p.speaks(METADATA) => {
                p.cluster = Cluster::Checking;
                jobs.push((*id, Job::Metadata));
                continue;
            }
            Cluster::Unknown => p.cluster = Cluster::Ok,
            Cluster::Checking => continue,
            _ => {}
        }
        if !p.speaks(FILTER_SUBSCRIBE) || p.filter_busy || p.filter_retry_at.is_some_and(|t| t > now) {
            continue;
        }
        let job = match &p.filter {
            None if !wanted.is_empty() && active < cfg.max_filter_peers => {
                active += 1;
                Some((fst::SUBSCRIBE, wanted.iter().cloned().collect()))
            }
            None => None,
            Some(_) if wanted.is_empty() => Some((fst::UNSUBSCRIBE_ALL, Vec::new())),
            Some(sub) => {
                let added: Vec<String> = wanted.difference(&sub.topics).cloned().collect();
                let removed: Vec<String> = sub.topics.difference(&wanted).cloned().collect();
                if !added.is_empty() {
                    Some((fst::SUBSCRIBE, added))
                } else if !removed.is_empty() {
                    Some((fst::UNSUBSCRIBE, removed))
                } else if sub.last_ok.elapsed() >= cfg.filter_ping_interval {
                    Some((fst::SUBSCRIBER_PING, Vec::new()))
                } else {
                    None
                }
            }
        };
        if let Some((kind, topics)) = job {
            p.filter_busy = true;
            jobs.push((*id, Job::Filter { kind, topics }));
        }
    }
    jobs
}

async fn run_filter_job(
    inner: &Inner,
    control: &mut libp2p_stream::Control,
    peer: PeerId,
    kind: i32,
    topics: Vec<String>,
) {
    let cfg = &inner.config;
    let mut result = Ok(());
    let chunks: Vec<Vec<String>> = if topics.is_empty() {
        vec![Vec::new()]
    } else {
        topics.chunks(filter::MAX_CONTENT_TOPICS_PER_REQUEST).map(<[String]>::to_vec).collect()
    };
    for chunk in chunks {
        let with_topics = matches!(kind, fst::SUBSCRIBE | fst::UNSUBSCRIBE);
        let req = FilterSubscribeRequest {
            request_id: inner.request_id(),
            filter_subscribe_type: kind,
            pubsub_topic: with_topics.then(|| cfg.pubsub_topic.clone()),
            content_topics: chunk,
        };
        result = filter::request(control, peer, req, cfg.request_timeout).await;
        if result.is_err() {
            break;
        }
    }

    let label = match kind {
        fst::SUBSCRIBE => "subscribe",
        fst::UNSUBSCRIBE => "unsubscribe",
        fst::UNSUBSCRIBE_ALL => "unsubscribe all",
        _ => "ping",
    };
    let mut st = inner.state();
    let Some(p) = st.peers.get_mut(&peer) else { return };
    p.filter_busy = false;
    match result {
        Ok(()) => {
            p.filter_retry_at = None;
            match kind {
                fst::SUBSCRIBE => {
                    let sub = p.filter.get_or_insert_with(|| Subscription { topics: BTreeSet::new(), last_ok: Instant::now() });
                    sub.topics.extend(topics);
                    sub.last_ok = Instant::now();
                    info!(%peer, topics = sub.topics.len(), "filter subscription active");
                    // Earlier dial or identity notices no longer describe the node's state.
                    st.last_error = None;
                }
                fst::UNSUBSCRIBE => {
                    if let Some(sub) = p.filter.as_mut() {
                        for t in &topics {
                            sub.topics.remove(t);
                        }
                        sub.last_ok = Instant::now();
                    }
                }
                fst::UNSUBSCRIBE_ALL => p.filter = None,
                _ => {
                    if let Some(sub) = p.filter.as_mut() {
                        sub.last_ok = Instant::now();
                    }
                }
            }
        }
        Err(e) => {
            warn!(%peer, error = %e, "filter {label} failed");
            // A failed ping means the service node dropped us: subscribe again from scratch.
            if kind == fst::SUBSCRIBER_PING || kind == fst::SUBSCRIBE {
                p.filter = None;
            }
            p.filter_retry_at = Some(Instant::now() + FILTER_RETRY);
            st.last_error = Some(format!("filter {label} on {peer}: {e}"));
        }
    }
    drop(st);
    inner.wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_pins_and_wss() {
        let d = Duration::from_secs(5);
        let b = split_bootstrap(
            &[
                "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x".parse().unwrap(),
                "/dns4/example.org/tcp/8000/wss".parse().unwrap(),
                "/ip4/127.0.0.1/tcp/60000/ws/p2p/16Uiu2HAmFbD2ZvAFi2j9jjDo6g4HFbQAhfjDfnTTrbyRGQRmtG7x".parse().unwrap(),
            ],
            d,
        )
        .unwrap();
        assert!(b[0].wss && b[0].pinned.is_some());
        assert_eq!(b[0].addr.to_string(), "/dns4/relay-a.rootedinprivacy.com/tcp/8000/wss");
        assert!(b[1].wss && b[1].pinned.is_none());
        assert!(!b[2].wss && b[2].pinned.is_some());
        // Plain ws without an identity: nothing would authenticate the peer.
        assert!(split_bootstrap(&["/ip4/127.0.0.1/tcp/60000/ws".parse().unwrap()], d).is_err());
    }
}
