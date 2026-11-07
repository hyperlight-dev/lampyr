// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! A template's service bindings: the Lampyr Services its project's
//! `bindings` names, which each fiber finds by their HTTP URLs in cluster
//! DNS, as `internal/project`'s BindingArgs and BindingResolver give them to
//! a local home.
//!
//! A binding `processor: worker-processor` becomes the guest variable
//! `LAMPYR_BINDING_PROCESSOR=http://worker-processor.NAMESPACE.svc.cluster.local`,
//! and the guest may send any TCP or UDP traffic to the addresses that host
//! resolves to, on any port. To learn them, it may also send DNS questions for its binding
//! names, and no others, over UDP to the Pod's nameservers that are not
//! link-local and the well-known public resolvers hyperlight-unikraft's
//! AllowList exempts; it reaches nothing else. A Lampyr Service's name
//! resolves to the cluster's shared internal ingress, so any binding
//! reaches every Knative Service through it.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyperlight_unikraft::{AllowList, NetworkPolicy, SandboxBuilder};

/// The namespace Lampyr deploys Services to, and so the one a binding's
/// target is in.
pub const NAMESPACE: &str = "default";

/// Where the grant Pod's own DNS configuration is, which a fiber that has
/// bindings resolves their hosts through.
pub const POD_RESOLV_CONF: &str = "/etc/resolv.conf";

/// How often a fiber waiting for its bindings to resolve looks for a
/// KILL.
const RESOLVE_POLL: Duration = Duration::from_millis(10);

/// What a template's bindings give each of its fibers.
#[derive(Debug, Default, PartialEq)]
pub struct Bindings {
    /// One `LAMPYR_BINDING_NAME` variable per binding, by name.
    env: Vec<(String, String)>,
    /// The hosts the guest may connect to: each binding's, fully
    /// qualified with a trailing dot.
    hosts: Vec<String>,
    /// The guest's resolv.conf, set only when there are bindings.
    resolv_conf: Option<String>,
}

impl Bindings {
    /// The bindings `declared`, which map a binding name to the Service it
    /// reaches in `namespace`. The Pod's DNS configuration is read only
    /// when there are some.
    pub fn new(
        declared: &BTreeMap<String, String>,
        namespace: &str,
        pod_resolv_conf: impl FnOnce() -> Result<String>,
    ) -> Result<Self> {
        ensure!(
            service_name(namespace),
            "invalid binding namespace {namespace:?}"
        );
        if declared.is_empty() {
            return Ok(Self::default());
        }
        let mut bindings = Self::default();
        // BTreeMap iterates by name, so the guest's variables are in order.
        for (name, service) in declared {
            ensure!(
                binding_name(name) && service_name(service),
                "invalid service binding {name:?}: {service:?}"
            );
            let host = format!("{service}.{namespace}.svc.cluster.local");
            bindings.env.push((
                format!("LAMPYR_BINDING_{}", name.to_ascii_uppercase()),
                format!("http://{host}"),
            ));
            // The helper resolves these on every CLONE and RESUME, through
            // the Pod's resolv.conf, whose ndots:5 would first ask for each
            // search-suffixed name. The trailing dot asks for the name
            // alone, and the allow list ignores it when it matches the
            // guest's questions.
            bindings.hosts.push(format!("{host}."));
        }
        bindings.resolv_conf = Some(resolver(&pod_resolv_conf()?)?);
        Ok(bindings)
    }

    /// One fiber's allow list: its binding hosts, resolved by `until`.
    /// None when `cancelled` (a KILL) comes first. A host that does not
    /// resolve, or does not by `until`, fails the fiber, so one whose
    /// binding is not deployed is refused, never let out.
    ///
    /// The allow list is made per fiber, since it learns the addresses its
    /// guest is told for the hosts, and a clone would share what it learns.
    pub fn resolve(&self, until: Instant, cancelled: impl Fn() -> bool) -> Result<Option<Policy>> {
        if self.resolv_conf.is_none() {
            return Ok(Some(Policy(None)));
        }
        let hosts = self.hosts.clone();
        let list = bounded(until, cancelled, move || Ok(AllowList::from_hosts(&hosts)?))?;
        Ok(list.map(|list| Policy(Some(list))))
    }

    /// `builder` with the bindings and the fiber's `policy` applied. A
    /// fresh clone gets their variables too; a resumed park keeps the ones
    /// it was cloned with.
    pub fn apply(&self, builder: SandboxBuilder, fresh: bool, policy: Policy) -> SandboxBuilder {
        self.configure(builder, fresh, policy.0)
    }

    fn configure<G: Guest>(&self, mut guest: G, fresh: bool, policy: Option<G::Policy>) -> G {
        // Without a policy the guest keeps the allow list listen ports
        // give it, which lists nothing.
        let (Some(resolv_conf), Some(policy)) = (&self.resolv_conf, policy) else {
            return guest;
        };
        if fresh {
            for (key, value) in &self.env {
                guest = guest.env(key, value);
            }
        }
        guest.allow(policy).resolv_conf(resolv_conf)
    }
}

/// One fiber's resolved allow list, None for a template without bindings.
pub struct Policy(Option<AllowList>);

/// `lookup`'s answer, or None once `cancelled`, or an error once `until`
/// passes. The system resolver cannot be interrupted, so the lookup runs
/// on a thread of its own, which a fiber that stops waiting leaves to end
/// on the resolver's own timeout: a slow Pod DNS then holds neither a
/// restore slot past its deadline nor a KILLed fiber.
fn bounded<T: Send + 'static>(
    until: Instant,
    cancelled: impl Fn() -> bool,
    lookup: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Option<T>> {
    let (answer, answered) = sync_channel(1);
    thread::Builder::new()
        .name("binding-dns".into())
        .spawn(move || {
            let _ = answer.send(lookup());
        })
        .context("start the service bindings' lookup")?;
    loop {
        if cancelled() {
            return Ok(None);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("the service bindings did not resolve before the deadline");
        }
        match answered.recv_timeout(left.min(RESOLVE_POLL)) {
            Ok(found) => return found.map(Some),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("the service bindings' lookup ended without an answer")
            }
        }
    }
}

/// What the bindings set on a sandbox, so the tests can see it: a
/// SandboxBuilder's settings are private.
trait Guest: Sized {
    /// What allows the guest its binding hosts alone.
    type Policy;
    fn env(self, key: &str, value: &str) -> Self;
    fn allow(self, policy: Self::Policy) -> Self;
    fn resolv_conf(self, conf: &str) -> Self;
}

impl Guest for SandboxBuilder {
    type Policy = AllowList;

    fn env(self, key: &str, value: &str) -> Self {
        SandboxBuilder::env(self, key, value)
    }

    fn allow(self, list: AllowList) -> Self {
        self.network(NetworkPolicy::AllowList(list))
    }

    fn resolv_conf(self, conf: &str) -> Self {
        SandboxBuilder::resolv_conf(self, conf)
    }
}

/// The Pod's nameservers the guest can reach, with ndots:1: binding hosts
/// are fully qualified, and Kubernetes' ndots:5 would first ask for
/// search-suffixed names the allow list does not name.
///
/// hyperlight-unikraft refuses guest traffic to link-local addresses
/// under every policy, as cloud metadata lives there, and it checks that
/// before the nameservers it exempts. A link-local nameserver, as AKS
/// LocalDNS and NodeLocal DNSCache give a Pod, would drop every question
/// the guest asks while the helper's own lookups still succeed, so it is
/// left out, and a Pod with no other fails the warm.
fn resolver(source: &str) -> Result<String> {
    let mut servers = Vec::new();
    let mut link_local = Vec::new();
    for line in source.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(given) = fields.next() else {
            bail!("invalid Pod DNS nameserver");
        };
        // A zone (`fe80::1%eth0`) only qualifies a link-local address.
        let (address, zone) = match given.split_once('%') {
            Some((address, zone)) => (address, Some(zone)),
            None => (given, None),
        };
        let Ok(server) = address.parse::<IpAddr>().map(|ip| ip.to_canonical()) else {
            bail!("invalid Pod DNS nameserver");
        };
        if is_link_local(&server) {
            link_local.push(given);
        } else if zone.is_some() {
            bail!("invalid Pod DNS nameserver");
        } else {
            servers.push(format!("nameserver {server}"));
        }
    }
    ensure!(
        !servers.is_empty() || link_local.is_empty(),
        "service bindings cannot use the Pod's link-local DNS nameservers ({}): \
         hyperlight-unikraft drops guest traffic to 169.254.0.0/16 and fe80::/10, \
         as AKS LocalDNS and NodeLocal DNSCache use; give grant Pods a nameserver \
         outside them",
        link_local.join(", ")
    );
    ensure!(
        !servers.is_empty(),
        "service bindings require Pod DNS nameservers"
    );
    Ok(format!("{}\noptions ndots:1\n", servers.join("\n")))
}

/// 169.254.0.0/16 or fe80::/10, as hyperlight-unikraft's metadata check
/// reads them.
fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// `^[a-z][a-z0-9_]*$`
fn binding_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A DNS label: `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`
fn service_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    const POD: &str = "search default.svc.cluster.local svc.cluster.local\n\
                       nameserver 10.96.0.10\noptions ndots:5\n";

    fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, service)| (name.to_string(), service.to_string()))
            .collect()
    }

    #[test]
    fn bindings_name_their_services_by_cluster_dns() {
        let bindings = Bindings::new(
            &declared(&[("processor", "worker-processor"), ("api_v2", "api")]),
            NAMESPACE,
            || Ok(POD.into()),
        )
        .unwrap();
        assert_eq!(
            bindings,
            Bindings {
                env: vec![
                    (
                        "LAMPYR_BINDING_API_V2".into(),
                        "http://api.default.svc.cluster.local".into()
                    ),
                    (
                        "LAMPYR_BINDING_PROCESSOR".into(),
                        "http://worker-processor.default.svc.cluster.local".into()
                    ),
                ],
                hosts: vec![
                    "api.default.svc.cluster.local.".into(),
                    "worker-processor.default.svc.cluster.local.".into(),
                ],
                resolv_conf: Some("nameserver 10.96.0.10\noptions ndots:1\n".into()),
            }
        );
    }

    #[test]
    fn a_template_without_bindings_never_reads_the_pod_s_dns() {
        let bindings =
            Bindings::new(&BTreeMap::new(), NAMESPACE, || bail!("read the Pod's DNS")).unwrap();
        assert_eq!(bindings, Bindings::default());
    }

    #[test]
    fn invalid_bindings_and_pod_dns_are_refused() {
        for (pairs, namespace, want) in [
            (
                &[("Processor", "worker")][..],
                NAMESPACE,
                "invalid service binding",
            ),
            (
                &[("9lives", "worker")],
                NAMESPACE,
                "invalid service binding",
            ),
            (
                &[("processor", "Worker")],
                NAMESPACE,
                "invalid service binding",
            ),
            (
                &[("processor", "-worker")],
                NAMESPACE,
                "invalid service binding",
            ),
            (
                &[("processor", "worker.other")],
                NAMESPACE,
                "invalid service binding",
            ),
            (
                &[("processor", "worker")],
                "Default",
                "invalid binding namespace",
            ),
        ] {
            let error = Bindings::new(&declared(pairs), namespace, || Ok(POD.into()))
                .unwrap_err()
                .to_string();
            assert!(error.contains(want), "{pairs:?} in {namespace}: {error}");
        }
        let long = "a".repeat(64);
        assert!(
            Bindings::new(&declared(&[("processor", &long)]), NAMESPACE, || Ok(
                POD.into()
            ))
            .is_err()
        );
        for (pod, want) in [
            (
                "search default.svc.cluster.local\n",
                "require Pod DNS nameservers",
            ),
            ("nameserver\n", "invalid Pod DNS nameserver"),
            ("nameserver kube-dns\n", "invalid Pod DNS nameserver"),
            ("nameserver 10.0.0.10%eth0\n", "invalid Pod DNS nameserver"),
            (
                "nameserver 169.254.10.10\nnameserver fe80::1%eth0\n",
                "cannot use the Pod's link-local DNS nameservers (169.254.10.10, fe80::1%eth0)",
            ),
            (
                "nameserver ::ffff:169.254.20.10\n",
                "cannot use the Pod's link-local DNS nameservers",
            ),
        ] {
            let error = Bindings::new(&declared(&[("processor", "worker")]), NAMESPACE, || {
                Ok(pod.into())
            })
            .unwrap_err()
            .to_string();
            assert!(error.contains(want), "{pod:?}: {error}");
        }
    }

    /// What `configure` set, in order.
    #[derive(Debug, Default, PartialEq)]
    struct Recorded(Vec<String>);

    impl Guest for Recorded {
        type Policy = Vec<String>;

        fn env(mut self, key: &str, value: &str) -> Self {
            self.0.push(format!("env {key}={value}"));
            self
        }

        fn allow(mut self, hosts: Vec<String>) -> Self {
            self.0.push(format!("allow {}", hosts.join(" ")));
            self
        }

        fn resolv_conf(mut self, conf: &str) -> Self {
            self.0.push(format!("resolv.conf {conf:?}"));
            self
        }
    }

    #[test]
    fn a_fresh_clone_gets_the_variables_and_a_resumed_park_only_the_policy() {
        let bindings = Bindings::new(&declared(&[("processor", "worker")]), NAMESPACE, || {
            Ok(POD.into())
        })
        .unwrap();
        let policy = [
            "allow worker.default.svc.cluster.local.".to_owned(),
            "resolv.conf \"nameserver 10.96.0.10\\noptions ndots:1\\n\"".to_owned(),
        ];
        let mut fresh =
            vec!["env LAMPYR_BINDING_PROCESSOR=http://worker.default.svc.cluster.local".to_owned()];
        fresh.extend(policy.clone());
        let hosts = || Some(bindings.hosts.clone());
        assert_eq!(
            bindings.configure(Recorded::default(), true, hosts()),
            Recorded(fresh)
        );
        assert_eq!(
            bindings.configure(Recorded::default(), false, hosts()),
            Recorded(policy.to_vec())
        );
    }

    #[test]
    fn a_template_without_bindings_sets_nothing() {
        let bindings = Bindings::default();
        for fresh in [true, false] {
            assert_eq!(
                bindings.configure(Recorded::default(), fresh, None),
                Recorded::default()
            );
        }
    }

    #[test]
    fn a_template_without_bindings_resolves_nothing() {
        let policy = Bindings::default()
            .resolve(Instant::now(), || panic!("waited"))
            .unwrap();
        assert!(matches!(policy, Some(Policy(None))));
    }

    #[test]
    fn a_lookup_answers_is_cancelled_or_runs_out_of_time() {
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(bounded(far, || false, || Ok(7)).unwrap(), Some(7));
        assert!(bounded(far, || false, || -> Result<()> { bail!("NXDOMAIN") }).is_err());
        let slow = || -> Result<()> {
            thread::sleep(Duration::from_secs(60));
            Ok(())
        };
        // A KILL ends the wait, and the deadline does, long before a
        // slow resolver answers.
        let started = Instant::now();
        assert_eq!(bounded(far, || true, slow).unwrap(), None);
        let soon = Instant::now() + Duration::from_millis(50);
        let error = bounded(soon, || false, slow).unwrap_err().to_string();
        assert!(
            error.contains("did not resolve before the deadline"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_resolver_keeps_every_nameserver_and_drops_the_rest() {
        assert_eq!(
            resolver("nameserver 10.0.0.10\nsearch x\nnameserver fd00::a\noptions ndots:5")
                .unwrap(),
            "nameserver 10.0.0.10\nnameserver fd00::a\noptions ndots:1\n"
        );
        // AKS LocalDNS beside the cluster DNS Service: the guest asks the
        // one it can reach.
        assert_eq!(
            resolver("nameserver 169.254.10.11\nnameserver 10.0.0.10\nnameserver fe80::1%eth0")
                .unwrap(),
            "nameserver 10.0.0.10\noptions ndots:1\n"
        );
    }
}
