// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ly", about = "Create, deploy, and operate Lampyr Services")]
struct Cli {
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct ServiceArgs {
    #[arg(default_value = ".")]
    directory: PathBuf,
    #[arg(long)]
    context: Option<String>,
}

#[derive(clap::Args)]
struct ClusterArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    /// Workspace root; infrastructure is stored under infra/PROVIDER/NAME.
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    provider_artifact: Option<PathBuf>,
    /// JSON/YAML settings for infrastructure and the shared Kubernetes platform.
    #[arg(long)]
    provider_config: Option<PathBuf>,
    #[arg(long)]
    context: Option<String>,
    /// Grant the provider access to the Docker daemon.
    #[arg(long)]
    allow_docker: bool,
    /// Apply provider-supplied Kubernetes manifests without prompting.
    #[arg(long)]
    approve_provider_manifests: bool,
    #[arg(long)]
    delete_cluster: bool,
    /// Install the shared Knative Eventing substrate.
    #[arg(long)]
    eventing: bool,
    #[arg(long)]
    non_interactive: bool,
}

#[derive(Subcommand)]
enum ClusterCommand {
    Install(ClusterArgs),
    Health(ClusterArgs),
    Uninstall(ClusterArgs),
}

#[derive(Subcommand)]
enum ScaffoldCommand {
    /// Create a Workspace containing services and infrastructure source.
    Workspace {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    /// Create a Lampyr Service under the current Workspace.
    Service {
        /// Service name; with --upgrade, also a path such as ./api or /srv/api.
        name: String,
        #[arg(long, conflicts_with = "upgrade")]
        runtime: Option<String>,
        /// Create a customer-owned application server instead of a Handler.
        #[arg(long, conflicts_with = "upgrade")]
        standalone: bool,
        /// Explicitly three-way merge the existing service's runtime adapter.
        #[arg(long)]
        upgrade: bool,
        /// Write conflict markers and pin the target for a later resolution.
        #[arg(long, requires = "upgrade")]
        write_conflicts: bool,
        /// Color conflict previews even when stderr is not a terminal.
        #[arg(long, value_enum, default_value = "auto")]
        color: lampyr_cli::adapter::Color,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Fetch prebuilt provider snapshot variants; never builds or publishes snapshots.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Forget one installation's dedicated cloud authentication cache.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Manage Lampyr on Kubernetes, optionally provisioning infrastructure.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Publish an event to a Lampyr Service.
    Event {
        #[command(subcommand)]
        command: EventCommand,
    },
    /// Install the independently packaged platform payload.
    Setup {
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        version: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long, conflicts_with = "remove")]
        check: bool,
        #[arg(long)]
        remove: bool,
    },
    /// Check platform discovery and local prerequisites.
    Doctor,
    /// Scaffold a Workspace or Lampyr Service.
    Scaffold {
        #[command(subcommand)]
        command: ScaffoldCommand,
    },
    /// Run a Service or Workspace locally with native Hyperlight snapshots.
    Dev {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long, default_value_t = 8080)]
        port: i32,
        /// Print one line per completed request and a summary on exit.
        #[arg(long)]
        metrics: bool,
    },
    /// Build the service snapshot and local OCI image.
    Build {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "linux-amd64-kvm")]
        target: String,
        #[arg(long, alias = "snapshot-context")]
        context: Option<String>,
    },
    /// Verify a local artifact's metadata and content digests.
    Verify {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    /// Publish a verified local artifact to a registry.
    Push {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long)]
        artifact: Option<PathBuf>,
        #[arg(long)]
        image: String,
    },
    /// Deploy an immutable artifact as a Lampyr Service.
    Deploy {
        #[command(flatten)]
        service: ServiceArgs,
        #[arg(long)]
        artifact: Option<PathBuf>,
        #[arg(long)]
        image: String,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long, default_value = "")]
        traffic: String,
    },
    /// Show the deployed service and its Fiber capacity.
    Status(ServiceArgs),
    /// Read structured application-handler logs.
    Logs {
        #[command(flatten)]
        service: ServiceArgs,
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
    /// Read Fiber lifecycle audit records.
    Audit {
        #[command(flatten)]
        service: ServiceArgs,
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
    /// Change the active revision's Fiber limit.
    Scale {
        #[command(flatten)]
        service: ServiceArgs,
        #[arg(long)]
        max_fibers: i32,
    },
    /// Remove a deployed Lampyr Service.
    Delete(ServiceArgs),
    /// Print the CLI version.
    Version,
}

#[derive(Subcommand)]
enum AuthCommand {
    Forget {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// Pull the compatible variant of a digest-pinned snapshot index into the local cache.
    PullSnapshots {
        /// Explicit HOST/REPOSITORY (HTTPS, or loopback HTTP for local registries).
        #[arg(long)]
        registry: String,
        /// Index digest from the provider's snapshotIndex.sha256, prefixed with sha256:.
        #[arg(long)]
        index: String,
    },
}

#[derive(Subcommand)]
enum EventCommand {
    /// Manage event sources through the selected Kubernetes context.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Deliver a CloudEvent, using retry and dead-letter policy when configured.
    Publish {
        #[command(flatten)]
        service: ServiceArgs,
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value = "/events")]
        path: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "{}")]
        data: String,
        #[arg(long)]
        insecure: bool,
    },
}

#[derive(clap::Args)]
struct EventSourceArgs {
    #[command(flatten)]
    service: ServiceArgs,
    /// Provider plugin that owns this event source.
    #[arg(long)]
    provider: String,
    #[arg(long, default_value = "default")]
    namespace: String,
    #[arg(long)]
    name: String,
}

#[derive(Subcommand)]
enum SourceCommand {
    Create(EventSourceArgs),
    Status(EventSourceArgs),
    Delete(EventSourceArgs),
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("ly: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let cancellation = lampyr_cli::runtime::Cancellation::default();
    let interrupted = cancellation.clone();
    ctrlc::set_handler(move || interrupted.cancel()).context("install signal handlers")?;
    let host = lampyr_cli::host::Host {
        verbose: cli.verbose,
        cancellation,
    };
    match cli.command {
        Command::Auth {
            command: AuthCommand::Forget { provider, name },
        } => {
            lampyr_cli::installation::forget_auth(&provider, &name)?;
            println!("forgot the dedicated {provider} authentication cache for {name}");
        }
        Command::Cluster { command } => run_cluster(command, &host)?,
        Command::Event {
            command: EventCommand::Source { command },
        } => run_event_source(command, &host)?,
        Command::Event {
            command:
                EventCommand::Publish {
                    service,
                    url,
                    path,
                    id,
                    data,
                    insecure,
                },
        } => {
            let delivered = lampyr_cli::event::publish(
                lampyr_cli::event::Options {
                    directory: &service.directory,
                    context: service.context.as_deref(),
                    url: url.as_deref(),
                    path: &path,
                    id: id.as_deref(),
                    data: &data,
                    insecure,
                },
                &host,
            )?;
            println!(
                "acknowledged event {} after {} attempt(s)",
                delivered.id, delivered.attempts
            );
        }
        Command::Setup {
            version,
            url,
            check,
            remove,
        } => {
            let installed = host.step("Preparing the Lampyr platform payload", || {
                lampyr_cli::platform::setup(
                    lampyr_cli::platform::Options {
                        version: &version,
                        url: url.as_deref(),
                        check,
                        remove,
                    },
                    &host,
                )
            })?;
            if !remove {
                println!("platform {}", installed.display());
            }
        }
        Command::Provider {
            command: ProviderCommand::PullSnapshots { registry, index },
        } => {
            let registry = lampyr_cli::provider_registry::SnapshotRegistry::new(
                &registry,
                lampyr_cli::provider_registry::SnapshotRegistry::default_cache()?,
            )?
            .with_cancellation(host.cancellation.clone());
            match registry
                .pull_selected(&index, &lampyr_cli::artifact::HostCompatibility::current())?
            {
                Some(layout) if layout.digest.is_some() => {
                    println!("snapshot layout {}", layout.path.display())
                }
                Some(_) => {
                    println!("no snapshot variant matches this host; provider will cold-boot")
                }
                None => println!("snapshot index absent; provider will cold-boot"),
            }
        }
        Command::Doctor => lampyr_cli::platform::doctor(None)?,
        Command::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        Command::Scaffold {
            command: ScaffoldCommand::Workspace { directory },
        } => {
            let root = lampyr_cli::customer_workspace::scaffold(&directory)?;
            println!("created Workspace {}", root.display());
        }
        Command::Scaffold {
            command:
                ScaffoldCommand::Service {
                    name,
                    runtime,
                    standalone,
                    upgrade,
                    write_conflicts,
                    color,
                },
        } => {
            let current = std::env::current_dir()?;
            // Build, dev, and deploy accept any service directory, so --upgrade
            // takes a path too; their drift warning prints one.
            let path_like = name.contains('/') || name.starts_with('.');
            if !(upgrade && path_like) {
                lampyr_cli::project::validate_name(&name)?;
            }
            let directory = if upgrade && path_like {
                current.join(&name)
            } else {
                match lampyr_cli::customer_workspace::find(&current)? {
                    Some(root) => root.join("services").join(&name),
                    None => current.join(&name),
                }
            };
            if upgrade {
                let directory = directory
                    .canonicalize()
                    .with_context(|| format!("find Lampyr Service at {}", directory.display()))?;
                lampyr_cli::adapter::upgrade(&directory, write_conflicts, color)
                    .with_context(|| format!("upgrade adapter at {}", directory.display()))?;
                println!(
                    "upgraded runtime adapter for {} at {}",
                    name,
                    directory.display()
                );
            } else {
                lampyr_cli::project::initialize_application(
                    &directory,
                    Some(&name),
                    runtime.as_deref().unwrap_or("nodejs"),
                    standalone,
                )?;
                println!("created Lampyr Service {} at {}", name, directory.display());
            }
        }
        Command::Dev {
            directory,
            port,
            metrics,
        } => {
            lampyr_cli::dev::run(&directory, None, port, metrics, &host)?;
        }
        Command::Build {
            directory,
            output,
            target,
            context,
        } => {
            let directories = build_directories(&directory)?;
            anyhow::ensure!(
                output.is_none() || directories.len() == 1,
                "--output cannot be used when building an entire Workspace"
            );
            for directory in directories {
                let built = lampyr_cli::build::build(
                    lampyr_cli::build::Options {
                        directory: &directory,
                        platform_root: None,
                        output: output.as_deref(),
                        target: &target,
                        context: context.as_deref(),
                    },
                    &host,
                )?;
                println!(
                    "built revision {}\nartifact {}\nmetadata {}\nimage {}",
                    built.metadata.revision_id,
                    built.directory.display(),
                    built.metadata_id,
                    built.image
                );
            }
        }
        Command::Verify { directory } => {
            let metadata = lampyr_cli::image::verify(&directory)?;
            println!(
                "verified revision {} for {}",
                metadata.revision_id, metadata.target.hypervisor
            );
        }
        Command::Push {
            directory,
            artifact,
            image,
        } => {
            let pushed = lampyr_cli::registry::push(
                lampyr_cli::registry::Options {
                    directory: &directory,
                    artifact: artifact.as_deref(),
                    image: &image,
                },
                &host,
            )?;
            println!("pushed image {pushed}");
        }
        Command::Deploy {
            service,
            artifact,
            image,
            namespace,
            traffic,
        } => {
            let deployed = lampyr_cli::deploy::deploy(
                lampyr_cli::deploy::Options {
                    directory: &service.directory,
                    artifact: artifact.as_deref(),
                    image: &image,
                    context: service.context.as_deref(),
                    namespace: &namespace,
                    traffic: &traffic,
                },
                &host,
            )?;
            println!(
                "deployed service {}\nrevision {}\nimage {}",
                deployed.service, deployed.revision, deployed.image
            );
            println!(
                "visibility {}",
                if deployed.exposed {
                    "external"
                } else {
                    "cluster-local"
                }
            );
            print_url(&deployed.url);
        }
        Command::Status(service) => {
            let status = lampyr_cli::lifecycle::status(
                &service.directory,
                service.context.as_deref(),
                &host,
            )?;
            println!("service {}\nrevision {}", status.service, status.revision);
            println!("application {}", status.application_model);
            print_url(&status.url);
            for target in status.traffic {
                println!("traffic {}={}%", target.revision, target.percent);
            }
            for capacity in status.capacity {
                println!("{}", capacity.describe());
            }
        }
        Command::Logs { service, tail } => {
            for record in lampyr_cli::lifecycle::logs(
                &service.directory,
                service.context.as_deref(),
                tail,
                false,
                &host,
            )? {
                println!("{record}");
            }
        }
        Command::Audit { service, tail } => {
            for record in lampyr_cli::lifecycle::logs(
                &service.directory,
                service.context.as_deref(),
                tail,
                true,
                &host,
            )? {
                println!("{record}");
            }
        }
        Command::Scale {
            service,
            max_fibers,
        } => {
            let name = lampyr_cli::lifecycle::scale(
                &service.directory,
                service.context.as_deref(),
                max_fibers,
                &host,
            )?;
            println!("scaled Grant {name} to {max_fibers} Fibers");
        }
        Command::Delete(service) => {
            lampyr_cli::lifecycle::delete(&service.directory, service.context.as_deref(), &host)?;
            println!("deleted {}", service.directory.display());
        }
    }

    fn print_url(url: &str) {
        if url.is_empty() {
            println!("url unavailable (configure the installation's domain)");
        } else {
            println!("url {url}");
        }
    }
    Ok(())
}

fn build_directories(directory: &std::path::Path) -> Result<Vec<PathBuf>> {
    let directory = std::path::absolute(directory)?;
    if directory.join("lampyr.yaml").is_file() {
        return Ok(vec![directory]);
    }
    let Some(root) = lampyr_cli::customer_workspace::find(&directory)? else {
        return Ok(vec![directory]);
    };
    if root != directory {
        return Ok(vec![directory]);
    }
    let services = lampyr_cli::customer_workspace::services(&root)?;
    anyhow::ensure!(
        !services.is_empty(),
        "Workspace contains no Lampyr Services"
    );
    Ok(services)
}

fn run_event_source(command: SourceCommand, host: &lampyr_cli::host::Host) -> Result<()> {
    use lampyr_cli::artifact::Operation;
    let (operation, source) = match &command {
        SourceCommand::Create(source) => (Operation::SourceCreate, source),
        SourceCommand::Status(source) => (Operation::SourceStatus, source),
        SourceCommand::Delete(source) => (Operation::SourceDelete, source),
    };
    lampyr_cli::event_source::run(
        operation,
        &source.provider,
        &source.service.directory,
        source.service.context.as_deref(),
        &source.namespace,
        &source.name,
        host,
    )?;
    Ok(())
}

fn run_cluster(command: ClusterCommand, host: &lampyr_cli::host::Host) -> Result<()> {
    use std::io::IsTerminal;
    let args = match &command {
        ClusterCommand::Install(args)
        | ClusterCommand::Health(args)
        | ClusterCommand::Uninstall(args) => args,
    };
    let operation = match command {
        ClusterCommand::Install(_) => lampyr_cli::artifact::Operation::Install,
        ClusterCommand::Health(_) => lampyr_cli::artifact::Operation::Health,
        ClusterCommand::Uninstall(_) => lampyr_cli::artifact::Operation::Uninstall,
    };
    anyhow::ensure!(
        !args.delete_cluster || operation == lampyr_cli::artifact::Operation::Uninstall,
        "--delete-cluster is only valid with cluster uninstall"
    );
    anyhow::ensure!(
        !args.eventing || operation == lampyr_cli::artifact::Operation::Install,
        "--eventing is only valid with cluster install"
    );
    let mut selected = None;
    let (provider, name, context) = if operation == lampyr_cli::artifact::Operation::Uninstall
        && (args.delete_cluster || args.provider.is_some())
        && (args.provider.is_none() || (args.name.is_none() && args.context.is_some()))
    {
        let selection = lampyr_cli::cluster::selection(args.context.as_deref(), None, host)?;
        if let Some(provider) = &args.provider {
            anyhow::ensure!(
                selection.provider.as_ref() == Some(provider),
                "selected context belongs to another provider"
            );
        }
        let result = (
            args.provider.clone().or_else(|| selection.provider.clone()),
            args.name.clone().or_else(|| selection.name.clone()),
            Some(selection.context.clone()),
        );
        selected = Some(selection);
        result
    } else {
        (
            args.provider.clone(),
            args.name.clone(),
            args.context.clone(),
        )
    };
    anyhow::ensure!(
        !args.delete_cluster || provider.is_some(),
        "--delete-cluster requires an infrastructure provider"
    );
    let mut config = lampyr_cli::cluster::read_config(args.provider_config.as_deref())?;
    anyhow::ensure!(
        config.get("eventing").is_none() && config.get("eventingEnabled").is_none(),
        "use --eventing instead of eventing fields in provider configuration"
    );
    config["deleteCluster"] = args.delete_cluster.into();
    config["eventing"] = args.eventing.into();
    let options = lampyr_cli::cluster::Options {
        name: name.as_deref(),
        provider: provider.as_deref(),
        root: None,
        workspace: args.workspace.as_deref(),
        artifact: args.provider_artifact.as_deref(),
        context: context.as_deref(),
        config,
        allow_docker: args.allow_docker,
        approve_provider_manifests: args.approve_provider_manifests,
        interactive: !args.non_interactive && std::io::stdin().is_terminal(),
        selection: selected.as_ref(),
    };
    lampyr_cli::cluster::run(operation, options, host)
}
