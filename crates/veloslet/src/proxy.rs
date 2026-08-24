//! The worker-side service proxy — Velos's `kube-proxy`.
//!
//! A container's address lives on the worker's own container network
//! (`192.168.64.0/24` under Apple Containerization). That network is reachable
//! from the worker Mac and nowhere else, and every worker is handed the *same*
//! range, so a container address is meaningless off-host. Something on the
//! worker has to translate a stable, externally reachable port into it.
//!
//! `container run --publish` is the obvious candidate and does not work:
//! apple/container 1.0.0 binds the host port and then fails to reach the
//! container behind it (`backend - connect failed: No route to host`), so a
//! published port accepts connections and drops them. Velos therefore does the
//! forwarding itself, in userspace, exactly as kube-proxy's original userspace
//! mode did.
//!
//! The pure half — [`plan_bindings`] — decides *what* should be listening from
//! the Services and the containers on this worker. [`NodeProxy`] is the
//! side-effecting half that makes the listeners match (Principle #5).
//!
//! A node port only listens on workers that actually run a selected container,
//! which is Kubernetes' `externalTrafficPolicy: Local`. That is what lets an
//! external load balancer point at every worker unconditionally: a worker with
//! no replica refuses the connection and drops out of rotation on its own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

/// One port a Service exposes, as the worker needs it: the port to listen on
/// and the port to forward to inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePortView {
    pub node_port: u16,
    pub target_port: u16,
}

/// A Service as the worker reads it off the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceView {
    pub name: String,
    pub selector: Vec<(String, String)>,
    pub ports: Vec<ServicePortView>,
}

/// A container the server has bound to this worker, joined with what the
/// runtime reports about its instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalContainer {
    pub labels: Vec<(String, String)>,
    /// The instance's address on the container network, when it is running and
    /// the runtime reports one.
    pub ip: Option<String>,
}

/// One listener this worker should be running, and where it should send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyBinding {
    pub node_port: u16,
    /// Which Service owns the port. A port reassigned to a different Service
    /// must be rebuilt rather than quietly repointed, so the owner is part of
    /// the binding's identity.
    pub service: String,
    /// `ip:port` of every local container answering for this port.
    pub backends: Vec<String>,
}

/// Whether a container's labels satisfy every entry of a selector. An empty
/// selector matches nothing: the server rejects one at admission, and reading it
/// as "match everything" would put every container on this worker behind a name
/// its owner never asked for.
fn selected(selector: &[(String, String)], labels: &[(String, String)]) -> bool {
    if selector.is_empty() {
        return false;
    }
    selector
        .iter()
        .all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
}

/// Pure: the listeners this worker should be running, given the cluster's
/// Services and the containers bound here.
///
/// A port with no local backend produces no binding at all — the listener is
/// torn down rather than left accepting connections it would have to drop. A
/// connection refused is a signal an upstream load balancer can act on; a
/// connection accepted and dropped looks like an application error.
pub fn plan_bindings(services: &[ServiceView], locals: &[LocalContainer]) -> Vec<ProxyBinding> {
    let mut out = Vec::new();
    for svc in services {
        let matched: Vec<&LocalContainer> = locals
            .iter()
            .filter(|c| selected(&svc.selector, &c.labels))
            .collect();
        for port in &svc.ports {
            let mut backends: Vec<String> = matched
                .iter()
                .filter_map(|c| c.ip.as_ref())
                .map(|ip| format!("{ip}:{}", port.target_port))
                .collect();
            if backends.is_empty() {
                continue;
            }
            backends.sort();
            backends.dedup();
            out.push(ProxyBinding {
                node_port: port.node_port,
                service: svc.name.clone(),
                backends,
            });
        }
    }
    out.sort_by_key(|b| b.node_port);
    out
}

// ---------------------------------------------------------------------------
// Actuation
// ---------------------------------------------------------------------------

/// A live listener plus the handle used to keep its backend list current.
struct Listener {
    service: String,
    backends: watch::Sender<Vec<String>>,
    /// `None` only while [`Listener::stop`] is taking the handle to await it.
    task: Option<JoinHandle<()>>,
}

impl Listener {
    /// Stop serving and wait until the socket is actually released.
    ///
    /// The await is load-bearing. `abort` only *requests* cancellation: the
    /// accept loop owns the socket, so the port stays bound until the task is
    /// polled and dropped. Rebinding it in the same pass — which is exactly what
    /// happens when a node port is reassigned to another Service — would
    /// otherwise fail with "address already in use" and leave the port dark.
    async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Listener {
    /// Backstop for the listeners still up when the whole proxy goes away.
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// The set of node-port listeners this worker is running.
#[derive(Default)]
pub struct NodeProxy {
    listeners: Mutex<HashMap<u16, Listener>>,
}

impl NodeProxy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ports currently being served (test/observability helper).
    pub async fn bound_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self.listeners.lock().await.keys().copied().collect();
        ports.sort();
        ports
    }

    /// Converge the running listeners onto `desired`.
    ///
    /// Updating an existing listener's backends is deliberately *not* a
    /// rebind: tearing the socket down and putting it back would drop every
    /// connection in flight each time a replica moved.
    ///
    /// A port that cannot be bound (something else holds it) is logged and left
    /// out; the next pass tries again. Failing the whole pass instead would take
    /// every other service on this worker down with it.
    pub async fn sync(&self, desired: Vec<ProxyBinding>) {
        let mut listeners = self.listeners.lock().await;
        let stale: Vec<u16> = listeners
            .iter()
            .filter(|(port, live)| {
                !desired
                    .iter()
                    .any(|b| b.node_port == **port && b.service == live.service)
            })
            .map(|(port, _)| *port)
            .collect();
        for port in stale {
            if let Some(live) = listeners.remove(&port) {
                tracing::info!(node_port = port, service = %live.service, "stopping service proxy");
                live.stop().await;
            }
        }

        for binding in desired {
            match listeners.get(&binding.node_port) {
                Some(live) => {
                    // `send` unconditionally: the receiver reads the value at
                    // accept time rather than waiting on a change, so a repeated
                    // value is harmless and a skipped one would be a stale route.
                    let _ = live.backends.send(binding.backends);
                }
                None => match TcpListener::bind(("0.0.0.0", binding.node_port)).await {
                    Ok(socket) => {
                        let (tx, rx) = watch::channel(binding.backends.clone());
                        let service = binding.service.clone();
                        let port = binding.node_port;
                        tracing::info!(
                            node_port = port,
                            service = %service,
                            backends = binding.backends.len(),
                            "serving service proxy"
                        );
                        let task = tokio::spawn(serve(socket, rx, service.clone(), port));
                        listeners.insert(
                            port,
                            Listener {
                                service,
                                backends: tx,
                                task: Some(task),
                            },
                        );
                    }
                    Err(e) => tracing::warn!(
                        node_port = binding.node_port,
                        service = %binding.service,
                        "cannot bind node port: {e}"
                    ),
                },
            }
        }
    }
}

/// Accept loop for one node port. Each connection is handed to the next backend
/// in round-robin order and proxied bidirectionally until either side closes.
async fn serve(
    socket: TcpListener,
    backends: watch::Receiver<Vec<String>>,
    service: String,
    node_port: u16,
) {
    let next = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut client, peer) = match socket.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // A failing accept can fail immediately and forever (fd
                // exhaustion), so back off rather than spin a core.
                tracing::warn!(node_port, service = %service, "accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Read the routes at accept time, so a reschedule takes effect on the
        // next connection without disturbing the ones already open.
        let live = backends.borrow().clone();
        if live.is_empty() {
            tracing::debug!(node_port, service = %service, "no backend for {peer}");
            continue;
        }
        let target = live[next.fetch_add(1, Ordering::Relaxed) % live.len()].clone();
        let service = service.clone();
        tokio::spawn(async move {
            match TcpStream::connect(&target).await {
                Ok(mut upstream) => {
                    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                    {
                        tracing::debug!(node_port, service = %service, "proxy stream ended: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!(node_port, service = %service, "backend {target} unreachable: {e}")
                }
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn svc(name: &str, selector: &[(&str, &str)], ports: &[(u16, u16)]) -> ServiceView {
        ServiceView {
            name: name.to_string(),
            selector: selector
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ports: ports
                .iter()
                .map(|(n, t)| ServicePortView {
                    node_port: *n,
                    target_port: *t,
                })
                .collect(),
        }
    }

    fn local(labels: &[(&str, &str)], ip: Option<&str>) -> LocalContainer {
        LocalContainer {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ip: ip.map(str::to_string),
        }
    }

    #[test]
    fn a_selected_running_container_becomes_a_backend() {
        let got = plan_bindings(
            &[svc("web", &[("app", "web")], &[(31000, 8080)])],
            &[local(&[("app", "web")], Some("192.168.64.5"))],
        );
        assert_eq!(
            got,
            vec![ProxyBinding {
                node_port: 31000,
                service: "web".to_string(),
                backends: vec!["192.168.64.5:8080".to_string()],
            }]
        );
    }

    #[test]
    fn replicas_on_one_worker_all_become_backends() {
        let got = plan_bindings(
            &[svc("web", &[("app", "web")], &[(31000, 8080)])],
            &[
                local(&[("app", "web")], Some("192.168.64.5")),
                local(&[("app", "web")], Some("192.168.64.6")),
            ],
        );
        assert_eq!(
            got[0].backends,
            vec!["192.168.64.5:8080", "192.168.64.6:8080"]
        );
    }

    #[test]
    fn a_container_without_an_address_is_not_a_backend() {
        // A hibernated or not-yet-started instance has no address. Binding the
        // port anyway would accept traffic with nowhere to send it, which reads
        // to a load balancer as a healthy backend returning errors.
        let got = plan_bindings(
            &[svc("web", &[("app", "web")], &[(31000, 8080)])],
            &[local(&[("app", "web")], None)],
        );
        assert!(got.is_empty());
    }

    #[test]
    fn labels_must_match_every_selector_entry() {
        let got = plan_bindings(
            &[svc(
                "web",
                &[("app", "web"), ("tier", "front")],
                &[(31000, 8080)],
            )],
            &[
                local(&[("app", "web")], Some("192.168.64.5")),
                local(&[("app", "web"), ("tier", "front")], Some("192.168.64.6")),
            ],
        );
        assert_eq!(got[0].backends, vec!["192.168.64.6:8080"]);
    }

    #[test]
    fn an_empty_selector_selects_nothing() {
        // Admission rejects an empty selector, so this can only arrive from a
        // document written before that rule or by another writer. Matching
        // everything would publish unrelated containers under this name.
        let got = plan_bindings(
            &[svc("web", &[], &[(31000, 8080)])],
            &[local(&[("app", "web")], Some("192.168.64.5"))],
        );
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn sync_binds_stops_and_rebuilds_on_reassignment() {
        let proxy = NodeProxy::new();
        proxy
            .sync(vec![ProxyBinding {
                node_port: 31111,
                service: "web".to_string(),
                backends: vec!["127.0.0.1:9".to_string()],
            }])
            .await;
        assert_eq!(proxy.bound_ports().await, vec![31111]);

        // Same port, different owner → the old listener must go, or the new
        // Service silently inherits the previous one's routes.
        proxy
            .sync(vec![ProxyBinding {
                node_port: 31111,
                service: "api".to_string(),
                backends: vec!["127.0.0.1:9".to_string()],
            }])
            .await;
        let held = proxy.listeners.lock().await;
        assert_eq!(held.get(&31111).map(|l| l.service.as_str()), Some("api"));
        drop(held);

        proxy.sync(Vec::new()).await;
        assert!(proxy.bound_ports().await.is_empty());
    }

    #[tokio::test]
    async fn a_bound_port_forwards_bytes_to_its_backend() {
        // The whole point of the module: bytes arriving on the node port come
        // out at the backend and back.
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = backend.accept().await.unwrap();
            let (mut r, mut w) = sock.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });

        let node_port = free_port().await;
        let proxy = NodeProxy::new();
        proxy
            .sync(vec![ProxyBinding {
                node_port,
                service: "echo".to_string(),
                backends: vec![backend_addr.to_string()],
            }])
            .await;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = TcpStream::connect(("127.0.0.1", node_port)).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
    }

    /// A port nothing is listening on, obtained by binding and releasing.
    async fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }
}
