// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use chrono::Utc;
use serde_json::json;

use crate::host::Host;
use crate::kube::Kube;
use crate::project::Project;

pub struct Options<'a> {
    pub directory: &'a Path,
    pub context: Option<&'a str>,
    pub url: Option<&'a str>,
    pub path: &'a str,
    pub id: Option<&'a str>,
    pub data: &'a str,
    pub insecure: bool,
}

pub struct Delivered {
    pub id: String,
    pub attempts: i32,
}

pub fn publish(options: Options<'_>, host: &Host) -> Result<Delivered> {
    let project = Project::load(options.directory)?;
    let direct = project.adapter_managed && !project.protocols.events;
    ensure!(
        project.protocols.events || direct,
        "events are not enabled for {}",
        project.name
    );
    ensure!(
        options.path.starts_with('/') && !options.path.chars().any(char::is_control),
        "event path must be an absolute HTTP path"
    );
    let id = match options.id {
        Some(id) => id.to_owned(),
        None => format!(
            "{:x}-{:x}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    };
    ensure!(!id.is_empty(), "event ID must not be empty");
    id.parse::<ureq::http::HeaderValue>()
        .context("invalid event ID")?;
    if !direct {
        crate::workspace::relative_path(&project.events.dead_letter)?;
    }
    let (url, insecure) = if let Some(url) = options.url {
        (url.to_owned(), options.insecure)
    } else {
        let kube = Kube::new(options.directory, options.context, host)?;
        let service: serde_json::Value =
            kube.get("default", "service.serving.knative.dev", &project.name)?;
        ensure!(
            !crate::kube::cluster_local(&service),
            "service is cluster-local; pass --url with a reachable endpoint, such as a port-forward URL"
        );
        let url = crate::lifecycle::service_status_url(&service, &kube.installation()?)?;
        ensure!(
            !url.is_empty(),
            "cluster has no public service domain; pass --url or configure the provider's domain"
        );
        implicit_destination(&url, options.path, options.insecure)?
    };
    ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "event URL must use http or https"
    );
    let uri: ureq::http::Uri = url.parse().context("invalid event URL")?;
    ensure!(
        uri.host().is_some()
            && uri
                .authority()
                .is_some_and(|value| !value.as_str().contains('@')),
        "event URL must have a host and no embedded credentials"
    );
    let backoff = if direct {
        Duration::ZERO
    } else {
        duration(&project.events.initial_backoff, "events.initialBackoff")?
    };
    let attempt_timeout = if project.events.attempt_timeout.is_empty() {
        Duration::from_secs(30)
    } else {
        duration(&project.events.attempt_timeout, "events.attemptTimeout")?
    };
    let agent = delivery_agent(attempt_timeout, insecure);
    let attempts = if direct {
        1
    } else {
        project.events.max_retries + 1
    };
    let mut failure = String::new();
    for attempt in 1..=attempts {
        ensure!(
            !host.cancellation.is_cancelled(),
            "event delivery cancelled"
        );
        let response = crate::host::interruptible_http(&agent, host, || {
            agent
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Ce-Specversion", "1.0")
                .header("Ce-Id", &id)
                .header("Ce-Type", "lampyr.event")
                .header("Ce-Source", "ly")
                .header("X-Lampyr-Delivery-Attempt", attempt.to_string())
                .send(options.data.as_bytes())
        })?;
        match response {
            Ok(response) if response.status().is_success() => {
                return Ok(Delivered {
                    id,
                    attempts: attempt,
                });
            }
            Ok(response) => {
                failure = format!("handler returned HTTP {}", response.status().as_u16())
            }
            Err(error) => failure = error.to_string(),
        }
        ensure!(
            !host.cancellation.is_cancelled(),
            "event delivery cancelled"
        );
        if attempt < attempts {
            let delay = backoff
                .checked_mul(1 << (attempt - 1))
                .context("event backoff is out of range")?;
            let until = Instant::now()
                .checked_add(delay)
                .context("event backoff deadline is out of range")?;
            while Instant::now() < until {
                ensure!(
                    !host.cancellation.is_cancelled(),
                    "event delivery cancelled"
                );
                std::thread::sleep(
                    until
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(25)),
                );
            }
        }
    }
    ensure!(!direct, "direct event {id} failed: {failure}");
    let append = (|| -> Result<()> {
        let path = Path::new(&project.events.dead_letter);
        crate::workspace::relative_path(&project.events.dead_letter)?;
        let directory = Dir::open_ambient_dir(options.directory, ambient_authority())?;
        if let Some(parent) = path.parent() {
            directory.create_dir_all(parent)?;
        }
        let mut settings = cap_std::fs::OpenOptions::new();
        settings.create(true).append(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            settings.mode(0o600);
        }
        let mut file = directory.open_with(path, &settings)?;
        let record = json!({ "id": id, "url": url, "data": options.data, "attempts": attempts, "error": failure, "failedAt": Utc::now().to_rfc3339() });
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        file.sync_all()?;
        Ok(())
    })();
    append.with_context(|| {
        format!("event delivery failed ({failure}) and dead-letter append failed")
    })?;
    bail!("event {id} failed after {attempts} attempts and was dead-lettered: {failure}")
}

fn duration(value: &str, field: &str) -> Result<Duration> {
    let nanos = go_parse_duration::parse_duration(value)
        .map_err(|error| anyhow::anyhow!("{field} is invalid: {error:?}"))?;
    ensure!(nanos > 0, "{field} must be positive");
    Ok(Duration::from_nanos(u64::try_from(nanos)?))
}

fn implicit_destination(base: &str, path: &str, insecure: bool) -> Result<(String, bool)> {
    let uri: ureq::http::Uri = base.parse().context("invalid deployed service URL")?;
    let local = uri
        .host()
        .is_some_and(|host| host == "localhost" || host.ends_with(".localhost"));
    Ok((format!("{base}{path}"), insecure || local))
}

fn delivery_agent(timeout: Duration, insecure: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_resolve(Some(Duration::from_secs(5).min(timeout)))
        .timeout_connect(Some(Duration::from_secs(10).min(timeout)))
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(insecure)
                .build(),
        )
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployed_remote_urls_honor_explicit_tls_policy() -> Result<()> {
        for base in [
            "https://service.default.corp.example",
            "https://localhost.example",
            "https://service.localhost.example",
            "https://192.0.2.1",
        ] {
            assert_eq!(
                implicit_destination(base, "/events", false)?,
                (format!("{base}/events"), false)
            );
            assert!(implicit_destination(base, "/events", true)?.1);
        }
        for base in [
            "https://localhost",
            "https://service.default.localhost:8443",
        ] {
            assert!(implicit_destination(base, "/events", false)?.1);
        }
        Ok(())
    }

    #[test]
    fn remote_self_signed_tls_is_rejected_unless_explicitly_insecure() -> Result<()> {
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::Arc;
        for insecure in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let (url, skip_verification) = implicit_destination(
                &format!("https://{}", listener.local_addr()?),
                "/events",
                insecure,
            )?;
            let key = rcgen::KeyPair::generate()?;
            let certificate = rcgen::CertificateParams::new(vec!["remote.invalid".to_owned()])?
                .self_signed(&key)?;
            let configuration = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )?;
            let server = std::thread::spawn(move || -> Result<()> {
                let (stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(configuration))?,
                    stream,
                );
                let mut request = [0; 4096];
                if stream.read(&mut request).is_ok() {
                    stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )?;
                }
                Ok(())
            });
            let result = delivery_agent(Duration::from_secs(2), skip_verification)
                .post(&url)
                .send(b"event");
            assert_eq!(result.is_ok(), insecure);
            server.join().unwrap()?;
        }
        Ok(())
    }
}
