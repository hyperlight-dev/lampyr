import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import signal
import subprocess
import sys
import tempfile
import time

import hyperlight
from lampyr_eventgrid import EventGrid
OWNER_TAG = "lampyr-installation-id"
ARM = "https://management.azure.com"
WORKSPACE = Path("/workspace")
TEMPLATES = Path("/templates")
PARAMETERS = "main.parameters.json"
EVENT_SOURCES = "event-sources.json"
PENDING = {"Accepted", "Running", "Creating", "Updating", "Deleting"}


class ProviderError(Exception):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def fail(code, message):
    raise ProviderError(code, message)


def progress(message):
    hyperlight.call("progress.message", message)




def guid(value):
    return isinstance(value, str) and re.fullmatch(
        r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", value,
    ) is not None


def validate_config(request):
    config = request.get("config")
    if not isinstance(config, dict):
        fail("invalid-config", "Azure configuration must be an object")
    # Persisted provider settings and shared platform fields accompany every lifecycle operation.
    allowed = {
        "name", "interactive", "context", "deleteCluster", "images", "fiberdImage",
        "routerImage", "nodeSetupImage", "domain", "httpsPort", "eventing",
        "subscriptionId", "tenantId", "resourceGroupName", "eventingResourceGroup",
        "location", "nodeVmSize", "nodeCount", "kubernetesVersion",
        "authenticationMode", "eventGridControllerImage", "eventGridAdapterImage",
        "eventSources", "teardown",
    }
    unknown = config.keys() - allowed
    if unknown:
        fail("invalid-config", "Unknown Azure configuration keys: " + ", ".join(sorted(map(str, unknown))))
    name = config.get("name")
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", name):
        fail("invalid-config", "Azure cluster name must be a DNS label")
    if not isinstance(request.get("installationId"), str) or not re.fullmatch(r"[0-9a-f]{32}", request["installationId"]):
        fail("invalid-config", "Invalid Azure installation identity")
    for key in ("subscriptionId", "tenantId"):
        if key in config and not guid(config[key]):
            fail("invalid-config", f"{key} must be a GUID")
    for key in ("interactive", "deleteCluster", "eventing", "teardown"):
        if key in config and type(config[key]) is not bool:
            fail("invalid-config", f"{key} must be a boolean")
    if config.get("authenticationMode", "browser") not in ("browser", "deviceCode"):
        fail("invalid-config", "authenticationMode must be browser or deviceCode")
    if "httpsPort" in config and config["httpsPort"] != 443:
        fail("invalid-config", "Azure supports only httpsPort 443")
    for key in ("resourceGroupName", "eventingResourceGroup"):
        if key in config and (
            not isinstance(config[key], str)
            or not re.fullmatch(r"[A-Za-z0-9_().-]{1,90}", config[key])
            or config[key].endswith(".")
        ):
            fail("invalid-config", f"{key} is not an Azure resource group name")
    if "nodeCount" in config and (type(config["nodeCount"]) is not int or not 1 <= config["nodeCount"] <= 100):
        fail("invalid-config", "nodeCount must be an integer from 1 to 100")
    for key in ("location", "nodeVmSize", "kubernetesVersion"):
        if key in config and (
            not isinstance(config[key], str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", config[key])
        ):
            fail("invalid-config", f"Invalid Azure {key}")
    if "context" in config and (
        not isinstance(config["context"], str) or not config["context"].strip()
        or config["context"].startswith("-") or any(ord(char) < 32 for char in config["context"])
    ):
        fail("invalid-config", "Invalid Kubernetes context")
    validate_event_sources(config.get("eventSources", {}))


def validate_event_sources(sources):
    if not isinstance(sources, dict) or len(sources) > 128:
        fail("invalid-config", "eventSources must be an object with at most 128 entries")
    for name, source in sources.items():
        if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", name):
            fail("invalid-config", "Event source names must be DNS labels")
        if not isinstance(source, dict):
            fail("invalid-config", f"eventSources.{name} must be an object")
        unknown = source.keys() - {
            "storageAccount", "sourceResourceId", "resourceGroup", "container", "eventTypes",
        }
        if unknown:
            fail("invalid-config", f"Unknown eventSources.{name} keys: " + ", ".join(sorted(map(str, unknown))))
        if ("storageAccount" in source) == ("sourceResourceId" in source):
            fail("invalid-config", f"eventSources.{name} requires exactly one of storageAccount or sourceResourceId")
        account = source.get("storageAccount")
        if account is not None and (
            not isinstance(account, str) or not re.fullmatch(r"[a-z0-9]{3,24}", account)
        ):
            fail("invalid-config", f"eventSources.{name}.storageAccount is invalid")
        resource = source.get("sourceResourceId")
        if resource is not None and (not isinstance(resource, str) or len(resource) > 1024):
            fail("invalid-config", f"eventSources.{name}.sourceResourceId is invalid")
        group_name = source.get("resourceGroup")
        if group_name is not None and "sourceResourceId" in source:
            fail("invalid-config", f"eventSources.{name}.resourceGroup cannot accompany sourceResourceId")
        if group_name is not None and (
            not isinstance(group_name, str)
            or not re.fullmatch(r"[A-Za-z0-9_().-]{1,90}", group_name)
            or group_name.endswith(".")
        ):
            fail("invalid-config", f"eventSources.{name}.resourceGroup is invalid")
        container = source.get("container")
        if container is not None and (
            not isinstance(container, str)
            or not (
                container == "$root"
                or (3 <= len(container) <= 63 and "--" not in container
                    and re.fullmatch(r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?", container))
            )
        ):
            fail("invalid-config", f"eventSources.{name}.container is invalid")
        event_types = source.get("eventTypes", ["Microsoft.Storage.BlobCreated"])
        if (
            not isinstance(event_types, list) or len(event_types) != 1
            or not isinstance(event_types[0], str) or not event_types[0].strip()
            or "," in event_types[0] or any(ord(char) < 32 for char in event_types[0])
        ):
            fail("invalid-config", f"eventSources.{name}.eventTypes must contain exactly one valid event type")


def state():
    return hyperlight.call("state.read", "cluster")


def save(value):
    hyperlight.call("state.write", {"key": "cluster", "value": value})


def stop_process(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def bounded_run(command, *, env=None, timeout, stdout_limit, stderr_limit):
    process = subprocess.Popen(
        command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + timeout
    selector = selectors.DefaultSelector()
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    limits = {"stdout": stdout_limit, "stderr": stderr_limit}
    for name in buffers:
        pipe = getattr(process, name)
        os.set_blocking(pipe.fileno(), False)
        selector.register(pipe, selectors.EVENT_READ, name)
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                stop_process(process)
                raise subprocess.TimeoutExpired(
                    command, timeout,
                    output=bytes(buffers["stdout"]),
                    stderr=bytes(buffers["stderr"]),
                )
            for key, _ in selector.select(min(0.1, remaining)):
                chunk = os.read(key.fileobj.fileno(), 64 * 1024)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                output = buffers[key.data]
                if len(output) + len(chunk) > limits[key.data]:
                    stop_process(process)
                    fail("output-limit", "Azure CLI output exceeded its limit")
                output.extend(chunk)
        remaining = max(0, deadline - time.monotonic())
        try:
            returncode = process.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            stop_process(process)
            raise subprocess.TimeoutExpired(
                command, timeout,
                output=bytes(buffers["stdout"]),
                stderr=bytes(buffers["stderr"]),
            )
    finally:
        selector.close()
        for name in buffers:
            getattr(process, name).close()
    return subprocess.CompletedProcess(
        command,
        returncode,
        bytes(buffers["stdout"]).decode("utf-8", errors="replace"),
        bytes(buffers["stderr"]).decode("utf-8", errors="replace"),
    )


def run(*args):
    return bounded_run(
        ["/bin/az", *args, "--only-show-errors", "--output", "json"],
        timeout=120,
        stdout_limit=8 * 1024 * 1024,
        stderr_limit=1024 * 1024,
    )


def az(*args, missing=False):
    result = run(*args)
    if result.returncode:
        if missing and re.search(
            r"ERROR:\s*\((NotFound|ResourceNotFound|ResourceGroupNotFound|DeploymentNotFound)\)",
            result.stderr,
        ):
            return None
        fail("azure-command", f"Azure {args[0]} {args[1]} failed: {result.stderr.strip()}")
    return json.loads(result.stdout) if result.stdout.strip() else None


def interactive_required(message):
    return any(marker in message for marker in (
        "Please run 'az login'", 'Please run "az login"', "interaction_required",
        "AADSTS50076", "AADSTS50079", "AADSTS700082", "AADSTS70043", "AADSTS50173",
    ))


def browser_login(tenant, interactive):
    if not interactive:
        fail("authentication-required", "The dedicated Azure provider cache requires login; run this command interactively to sign in. Host az login does not authenticate this cache.")
    progress("Open the following sign-in URL in a browser on this workstation to authenticate the Azure provider's dedicated cache.")
    args = ["/bin/az", "login", "--only-show-errors", "--output", "none"]
    if tenant:
        args += ["--tenant", tenant]
    environment = os.environ.copy()
    environment["BROWSER"] = shlex.join([
        sys.executable, "-c", "import sys; print(sys.argv[1], flush=True)", "%s",
    ])
    environment["AZURE_CORE_LOGIN_EXPERIENCE_V2"] = "off"
    environment["PYTHONUNBUFFERED"] = "1"
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=environment)
    if stream_login(process, 900) != 0:
        fail("authentication-failed", "Azure browser login failed")


def device_code_login(tenant, interactive):
    if not interactive:
        fail("authentication-required", "The dedicated Azure provider cache requires login; run this command interactively to sign in. Host az login does not authenticate this cache.")
    progress("Use the following device code to authenticate the Azure provider's dedicated cache.")
    args = ["/bin/az", "login", "--use-device-code", "--only-show-errors", "--output", "none"]
    if tenant:
        args += ["--tenant", tenant]
    environment = os.environ.copy()
    environment["AZURE_CORE_LOGIN_EXPERIENCE_V2"] = "off"
    environment["PYTHONUNBUFFERED"] = "1"
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=environment)
    if stream_login(process, 900) != 0:
        fail("authentication-failed", "Azure device-code login failed")


def authentication_mode(config, expected=None):
    return config.get("authenticationMode", (expected or {}).get("authenticationMode", "browser"))


def interactive_login(config, tenant, expected=None):
    if authentication_mode(config, expected) == "deviceCode":
        device_code_login(tenant, config.get("interactive") is True)
    else:
        browser_login(tenant, config.get("interactive") is True)


def stream_login(process, timeout):
    deadline = time.monotonic() + timeout
    descriptor = process.stdout.fileno()
    os.set_blocking(descriptor, False)
    selector = selectors.DefaultSelector()
    selector.register(descriptor, selectors.EVENT_READ)
    buffered = b""
    try:
        while process.poll() is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                fail("authentication-timeout", "Azure login exceeded its deadline")
            for _, _ in selector.select(min(1, remaining)):
                chunk = os.read(descriptor, 8192)
                if not chunk:
                    continue
                buffered += chunk
                if len(buffered) > 1024 * 1024:
                    process.kill()
                    process.wait(timeout=5)
                    fail("output-limit", "Azure login output exceeded its limit")
                lines = buffered.split(b"\n")
                buffered = lines.pop()
                for line in lines:
                    message = line.decode(errors="replace").strip()
                    if message:
                        progress(message[:8192])
        while True:
            try:
                chunk = os.read(descriptor, 8192)
            except BlockingIOError:
                break
            if not chunk:
                break
            buffered += chunk
        message = buffered.decode(errors="replace").strip()
        if message:
            progress(message[:8192])
        return process.wait(timeout=5)
    finally:
        selector.close()
        process.stdout.close()


def authenticate(request, expected=None, allow_login=True):
    config = request["config"]
    mode = authentication_mode(config, expected)
    subscription = expected["subscriptionId"] if expected else config.get("subscriptionId")
    tenant = expected["tenantId"] if expected else config.get("tenantId")
    args = ["account", "show"]
    if subscription:
        args += ["--subscription", subscription]
    result = run(*args)
    if result.returncode and interactive_required(result.stderr):
        if not allow_login:
            fail("authentication-failed", result.stderr.strip())
        interactive_login(config, tenant, expected)
        return authenticate(request, expected, False)
    if result.returncode:
        fail("authentication-failed", result.stderr.strip())
    account = json.loads(result.stdout)
    identity = {
        "subscriptionId": account["id"], "tenantId": account["tenantId"],
        "identity": account["user"]["name"], "identityType": account["user"]["type"],
        "authenticationMode": mode,
    }
    if account.get("environmentName") != "AzureCloud" or account.get("state") != "Enabled":
        fail("invalid-account", "The provider requires an enabled public Azure subscription")
    if not identity["identity"] or (tenant and identity["tenantId"].lower() != tenant.lower()):
        fail("identity-mismatch", "Azure login does not match the configured tenant")
    if subscription and identity["subscriptionId"].lower() != subscription.lower():
        fail("identity-mismatch", "Azure login does not match the configured subscription")
    if expected:
        for name in ("subscriptionId", "tenantId"):
            if identity[name].lower() != expected[name].lower():
                fail("identity-mismatch", "Azure tenant, subscription or identity changed since initialization")
        for name in ("identity", "identityType"):
            if identity[name] != expected[name]:
                fail("identity-mismatch", "Azure tenant, subscription or identity changed since initialization")
    token_args = ["account", "get-access-token", "--subscription", identity["subscriptionId"], "--resource", ARM + "/"]
    token = run(*token_args)
    if token.returncode and interactive_required(token.stderr):
        if not allow_login:
            fail("authentication-failed", token.stderr.strip())
        interactive_login(config, identity["tenantId"], identity)
        return authenticate(request, identity, False)
    if token.returncode:
        fail("authentication-failed", token.stderr.strip())
    if not json.loads(token.stdout).get("accessToken"):
        fail("authentication-failed", "Azure CLI returned no access token")
    return identity


def load_parameters(workspace=None):
    document = json.loads(((workspace or WORKSPACE) / PARAMETERS).read_text(encoding="utf-8"))
    parameters = {name: value["value"] for name, value in document["parameters"].items()}
    if "lampyrSourceDigest" in parameters:
        fail("invalid-source", "lampyrSourceDigest is reserved for binding a deployment to its current source")
    return parameters


def validate_parameters(parameters):
    for name in ("resourceGroupName", "eventingResourceGroup"):
        value = parameters[name]
        if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_().-]{1,90}", value) or value.endswith("."):
            fail("invalid-source", f"{name} is not an Azure resource group name")
    if type(parameters["eventingEnabled"]) is not bool:
        fail("invalid-source", "eventingEnabled must be a boolean")
    if type(parameters["nodeCount"]) is not int or not 1 <= parameters["nodeCount"] <= 100:
        fail("invalid-source", "nodeCount must be an integer from 1 to 100")
    if parameters["eventingResourceGroup"].lower() == parameters["resourceGroupName"].lower():
        fail("invalid-source", "Eventing must use a separate, dedicated resource group")


def settings(request, workspace=None, teardown=False):
    saved = state()
    if not saved or saved["installationId"] != request["installationId"]:
        fail("not-owned", "This workspace has no matching Azure installation state")
    parameters = dict(saved["parameters"]) if teardown else load_parameters(workspace)
    validate_parameters(parameters)
    if request["config"]["name"] != parameters["clusterName"]:
        fail("identity-mismatch", "The request targets a different cluster name")
    for name in ("installationId", "subscriptionId", "tenantId", "clusterName", "resourceGroupName"):
        same = (
            parameters[name].lower() == saved["parameters"][name].lower()
            if name in ("subscriptionId", "tenantId")
            else parameters[name] == saved["parameters"][name]
        )
        if not same:
            fail("identity-mismatch", f"{name} changed; refusing to target different Azure infrastructure")
    if parameters["eventingResourceGroup"] != saved["parameters"]["eventingResourceGroup"]:
        fail("identity-mismatch", "The dedicated Eventing resource group changed")
    for name, value in request["config"].items():
        same = (
            isinstance(value, str) and value.lower() == parameters[name].lower()
            if name in ("subscriptionId", "tenantId") and name in parameters
            else value == parameters.get(name)
        )
        if name in parameters and not same:
            fail("source-conflict", f"{name} differs from {PARAMETERS}; edit customer source instead of overriding it")
    identity = authenticate(request, saved["auth"])
    if identity != saved["auth"]:
        saved["auth"] = identity
        save(saved)
    return parameters, saved


def resource_id(parameters, *parts, group=None):
    group = group or parameters["resourceGroupName"]
    return "/subscriptions/{}/resourceGroups/{}/{}".format(
        parameters["subscriptionId"], group, "/".join(parts),
    ).rstrip("/")


def owned(resource, request, description):
    if resource.get("tags", {}).get(OWNER_TAG) != request["installationId"]:
        fail("not-owned", f"{description} is not owned by this Lampyr installation")


def cluster(parameters):
    current = az(
        "aks", "show", "--subscription", parameters["subscriptionId"],
        "--resource-group", parameters["resourceGroupName"], "--name", parameters["clusterName"],
        missing=True,
    )
    expected = resource_id(parameters, "providers", "Microsoft.ContainerService", "managedClusters", parameters["clusterName"])
    if current is not None and current.get("id", "").lower() != expected.lower():
        fail("identity-mismatch", "Azure returned a cluster outside this installation's scope")
    return current


def group(parameters, name):
    return az("group", "show", "--subscription", parameters["subscriptionId"], "--name", name, missing=True)


def compile_template(name, destination, workspace=None):
    progress(f"Compiling Azure infrastructure template {name}...")
    az("bicep", "build", "--file", str((workspace or WORKSPACE) / name), "--outfile", str(destination))
    progress(f"Compiled Azure infrastructure template {name}.")


def source_files():
    count = 0
    size = 0
    if not WORKSPACE.is_dir():
        fail("invalid-source", "Infrastructure workspace does not exist")
    for root, directories, files in os.walk(WORKSPACE, followlinks=False):
        directories[:] = sorted(name for name in directories if name not in (".git", ".hg", ".svn"))
        for name in directories + files:
            path = Path(root) / name
            count += 1
            if count > 1024:
                fail("source-limit", "Infrastructure source exceeds its file limit")
            if path.is_symlink():
                fail("invalid-source", "Infrastructure source must not contain symbolic links")
        for name in sorted(files):
            path = Path(root) / name
            relative = path.relative_to(WORKSPACE)
            if relative == Path(EVENT_SOURCES):
                continue
            if not path.is_file():
                fail("invalid-source", "Infrastructure source requires regular files")
            with path.open("rb") as source:
                content = source.read(16 * 1024 * 1024 - size + 1)
            size += len(content)
            if size > 16 * 1024 * 1024:
                fail("source-limit", "Infrastructure source exceeds its size limit")
            yield relative, content


def source_digest(destination=None):
    checksum = hashlib.sha256()
    if destination:
        destination.mkdir()
    for relative, content in source_files():
        checksum.update(relative.as_posix().encode() + b"\0")
        checksum.update(len(content).to_bytes(8, "big") + content)
        if destination:
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
    return checksum.hexdigest()


def latest_version(versions):
    candidates = set()
    for entry in versions["values"]:
        if entry.get("isPreview"):
            continue
        if re.fullmatch(r"\d+\.\d+\.\d+", entry.get("version", "")):
            candidates.add(entry["version"])
        for version, patch in entry.get("patchVersions", {}).items():
            if not patch.get("isPreview") and re.fullmatch(r"\d+\.\d+\.\d+", version):
                candidates.add(version)
    if not candidates:
        fail("no-kubernetes-version", "Azure returned no regional GA Kubernetes versions")
    return max(candidates, key=lambda value: tuple(map(int, value.split("."))))


def initialize(request):
    validate_config(request)
    saved = state()
    if saved:
        if saved["installationId"] != request["installationId"]:
            fail("not-owned", "Azure initialization belongs to another installation")
        if (WORKSPACE / PARAMETERS).is_file():
            parameters, _ = settings(request)
            config = request["config"]
            if config.get("eventing") is True and not parameters["eventingEnabled"]:
                fail(
                    "configuration-mismatch",
                    "Azure Eventing is disabled in the customer Workspace; set "
                    "eventingEnabled to true in main.parameters.json before rerunning "
                    "cluster install --eventing",
                )
            if "eventSources" in config and config["eventSources"] != source_definitions():
                fail(
                    "configuration-mismatch",
                    "provider-config eventSources differs from the customer Workspace; "
                    f"edit {EVENT_SOURCES} instead of changing provider config on reinstall",
                )
            return {"workspace": "main.bicep"}
        if saved["phase"] != "initialized" or any(WORKSPACE.iterdir()):
            fail("incomplete-source", "Azure infrastructure source is incomplete; restore the customer files instead of regenerating them")
    config = request["config"]
    if not saved and not config.get("subscriptionId"):
        fail("invalid-config", "Azure initialization requires subscriptionId in provider config")
    identity = authenticate(request, saved["auth"] if saved else None)
    location = config.get("location", "westus3")
    version = config.get("kubernetesVersion")
    if not version:
        versions = az("aks", "get-versions", "--subscription", identity["subscriptionId"], "--location", location)
        version = latest_version(versions)
    name = config["name"]
    parameters = {
        "installationId": request["installationId"], "subscriptionId": identity["subscriptionId"],
        "tenantId": identity["tenantId"], "clusterName": name,
        "resourceGroupName": config.get("resourceGroupName", name),
        "location": location, "kubernetesVersion": version,
        "nodeVmSize": config.get("nodeVmSize", "Standard_D4s_v5"),
        "nodeCount": config.get("nodeCount", 1), "eventingEnabled": config.get("eventing", False),
        "eventingResourceGroup": config.get("eventingResourceGroup", f"{name}-eventing-{request['installationId'][:10]}"),
    }
    validate_parameters(parameters)
    files = {
        path.relative_to(TEMPLATES).as_posix(): path.read_text(encoding="utf-8")
        for path in sorted(TEMPLATES.rglob("*.bicep")) if path.is_file()
    }
    files[PARAMETERS] = json.dumps({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
        "contentVersion": "1.0.0.0", "parameters": {name: {"value": value} for name, value in parameters.items()},
    }, indent=2) + "\n"
    files[EVENT_SOURCES] = json.dumps(config.get("eventSources", {}), indent=2) + "\n"
    save({
        "installationId": request["installationId"], "auth": identity,
        "parameters": parameters, "phase": "initialized",
        "deployment": f"lampyr-{request['installationId'][:20]}",
    })
    hyperlight.call("workspace.scaffold", files)
    progress(f"Using Azure tenant {identity['tenantId']}, subscription {identity['subscriptionId']}, identity {identity['identity']}.")
    return {"workspace": "main.bicep"}


def ensure_group(parameters, name, request, scratch, dedicated, workspace=None):
    existing = group(parameters, name)
    if existing:
        if dedicated:
            owned(existing, request, f"Eventing resource group {name}")
        if existing.get("properties", {}).get("provisioningState") == "Deleting":
            fail("deleting", f"Azure resource group {name} is still deleting; retry after deletion completes")
        if not dedicated and existing.get("tags", {}).get(OWNER_TAG) != request["installationId"]:
            return
    compile_template("resource-group.bicep", scratch / "group.json", workspace)
    progress(f"Creating or updating Azure resource group {name}...")
    az(
        "deployment", "sub", "create", "--subscription", parameters["subscriptionId"],
        "--name", f"lampyr-group-{hashlib.sha256(name.encode()).hexdigest()[:20]}",
        "--location", parameters["location"], "--template-file", str(scratch / "group.json"),
        "--parameters", f"name={name}", f"location={parameters['location']}",
        f"installationId={request['installationId']}",
    )
    owned(group(parameters, name) or {}, request, f"resource group {name}")


def deployment(parameters, saved):
    return az(
        "deployment", "group", "show", "--subscription", parameters["subscriptionId"],
        "--resource-group", parameters["resourceGroupName"], "--name", saved["deployment"], missing=True,
    )


def wait_deployment(parameters, saved, digest):
    last = None
    while True:
        current = deployment(parameters, saved)
        if current is None:
            fail("missing-operation", "The recorded Azure deployment disappeared")
        properties = current["properties"]
        if properties["parameters"]["lampyrSourceDigest"]["value"] != digest:
            fail("operation-conflict", "The Azure deployment is not bound to this customer source")
        phase = properties["provisioningState"]
        if phase != last:
            progress(f"Azure deployment {saved['deployment']}: {phase}")
            last = phase
        if phase == "Succeeded":
            return properties["outputs"]["lampyr"]["value"]
        if phase not in PENDING:
            fail("deployment-failed", f"Azure deployment {saved['deployment']} ended in {phase}: {json.dumps(properties.get('error', {}))}")
        time.sleep(5)


def deployment_scope(parameters, request):
    return {
        "installationId": request["installationId"],
        "subscriptionId": parameters["subscriptionId"], "tenantId": parameters["tenantId"],
        "clusterName": parameters["clusterName"], "resourceGroup": parameters["resourceGroupName"],
        "clusterId": resource_id(parameters, "providers", "Microsoft.ContainerService", "managedClusters", parameters["clusterName"]),
    }


def wait_cluster(parameters, request):
    last = None
    while True:
        current = cluster(parameters)
        if not current:
            return None
        owned(current, request, "AKS cluster")
        phase = current.get("provisioningState")
        if phase == "Deleting":
            fail("deleting", "AKS is still deleting; retry after deletion completes")
        if phase not in PENDING:
            return current
        if phase != last:
            progress(f"Waiting for AKS cluster {parameters['clusterName']}: {phase}")
            last = phase
        time.sleep(5)


def validate_outputs(parameters, request, result, digest):
    expected = dict(deployment_scope(parameters, request), sourceDigest=digest)
    for name, value in expected.items():
        if result.get(name) != value:
            fail("invalid-output", f"Azure deployment output {name} does not match this installation")
    current = wait_cluster(parameters, request)
    if not current:
        fail("invalid-output", "Azure deployment returned a missing AKS cluster")
    owned(current, request, "AKS cluster")
    if current.get("provisioningState") != "Succeeded":
        fail("invalid-output", "AKS is not ready after deployment")
    eventing = result.get("eventing", {})
    if not isinstance(eventing, dict) or bool(eventing) != parameters["eventingEnabled"]:
        fail("invalid-output", "Azure Eventing outputs do not match customer source")
    if parameters["eventingEnabled"]:
        eventing_inventory(parameters, request, result)
        if eventing.get("resourceGroup") != parameters["eventingResourceGroup"]:
            fail("invalid-output", "Azure Eventing output has a different deletion scope")
        owned(group(parameters, parameters["eventingResourceGroup"]) or {}, request, "Eventing resource group")
    return result


def apply(request):
    with tempfile.TemporaryDirectory(prefix=".lampyr-azure-", dir=".") as temporary:
        scratch = Path(temporary)
        source = scratch / "source"
        digest = source_digest(source)
        return apply_source(request, source, scratch, digest)


def apply_source(request, source, scratch, digest):
    parameters, saved = settings(request, source)
    if saved.get("outputs", {}).get("eventing") and not parameters["eventingEnabled"]:
        fail("eventing-removal-required",
             "Disabling Eventing requires full owned infrastructure deletion with --delete-cluster "
             "(destructive: deletes AKS), then re-provisioning with Eventing disabled. "
             "Uninstalling while retaining the cluster does not clear the saved Eventing outputs.")
    images = runtime_images(request, parameters["eventingEnabled"])
    wait_cluster(parameters, request)
    if parameters["eventingEnabled"]:
        existing = group(parameters, parameters["eventingResourceGroup"])
        if existing:
            owned(existing, request, "Eventing resource group")
            if existing.get("properties", {}).get("provisioningState") == "Deleting":
                fail("deleting", "The dedicated Eventing resource group is still deleting")
    current = deployment(parameters, saved)
    state = current["properties"]["provisioningState"] if current else None
    resumable = (
        saved["phase"] in ("starting", "deploying") and current
        and (state in PENDING or state == "Succeeded")
        and current["properties"].get("parameters", {}).get("lampyrSourceDigest", {}).get("value") == digest
    )
    if current and state in PENDING and not resumable:
        fail("operation-conflict", "A previous Azure deployment is still running; inspect it before applying changed source")
    if not resumable:
        compile_template("main.bicep", scratch / "main.json", source)
        ensure_group(parameters, parameters["resourceGroupName"], request, scratch, False, source)
        if parameters["eventingEnabled"]:
            ensure_group(parameters, parameters["eventingResourceGroup"], request, scratch, True, source)
        saved.update(phase="starting", sourceDigest=digest)
        save(saved)
        progress(f"Submitting Azure deployment {saved['deployment']}...")
        for attempt in range(120):
            try:
                az(
                    "deployment", "group", "create", "--subscription", parameters["subscriptionId"],
                    "--resource-group", parameters["resourceGroupName"], "--name", saved["deployment"],
                    "--mode", "Incremental", "--template-file", str(scratch / "main.json"),
                    "--parameters", "@" + str(source / PARAMETERS), f"lampyrSourceDigest={digest}", "--no-wait",
                )
                break
            except ProviderError as error:
                if (error.code != "azure-command" or "OperationNotAllowed" not in str(error)
                        or "in-progress update managed cluster operation" not in str(error)
                        or attempt == 119):
                    raise
                if wait_cluster(parameters, request) is None:
                    fail("missing-operation", "The owned AKS cluster disappeared during reconciliation")
                progress("Waiting for Azure to finish the owned AKS update before deployment")
                time.sleep(5)
        saved["phase"] = "deploying"
        save(saved)
    result = validate_outputs(parameters, request, wait_deployment(parameters, saved, digest), digest)
    saved.update(phase="ready", parameters=parameters, outputs=result)
    save(saved)
    hyperlight.call("state.write", {"key": "runtime", "value": runtime_manifests(
        request, parameters, result, images,
    )})
    return result


def inspect(request):
    parameters, saved = settings(request, teardown=request["config"].get("teardown") is True)
    current = cluster(parameters)
    if current:
        owned(current, request, "AKS cluster")
    remote = deployment(parameters, saved)
    return {
        "exists": current is not None, "owned": current is not None,
        "clusterId": current["id"] if current else None,
        "ready": current is not None and current.get("provisioningState") == "Succeeded",
        "clusterState": current.get("provisioningState") if current else None,
        "phase": saved["phase"], "deployment": saved["deployment"],
        "deploymentState": remote["properties"]["provisioningState"] if remote else None,
        "scope": deployment_scope(parameters, request),
    }


def connection_credentials(config):
    if len(config.get("users", [])) != 1:
        fail("invalid-connection", "AKS did not return exactly one credential")
    user = config["users"][0]["user"]
    if "auth-provider" in user:
        fail("invalid-connection", "AAD AKS credentials are not supported by this provider")
    elif user.get("client-certificate-data") and user.get("client-key-data"):
        unknown = user.keys() - {"client-certificate-data", "client-key-data", "token"}
        if unknown:
            fail("invalid-connection", "AKS returned unsupported credential fields: " + ", ".join(sorted(unknown)))
        # AKS can include a redundant token alongside its client certificate.
        user.pop("token", None)
    else:
        fail("invalid-connection", "AKS did not return embedded client certificate credentials")




def connect(request):
    parameters, saved = settings(request, teardown=True)
    import yaml
    current = cluster(parameters)
    if not current:
        fail("not-found", "The AKS cluster does not exist")
    owned(current, request, "AKS cluster")
    context = request["config"].get("context", f"aks-{parameters['clusterName']}")
    with tempfile.TemporaryDirectory(prefix=".lampyr-connection-", dir=".") as temporary:
        path = Path(temporary) / "kubeconfig"
        az(
            "aks", "get-credentials", "--subscription", parameters["subscriptionId"],
            "--resource-group", parameters["resourceGroupName"], "--name", parameters["clusterName"],
            "--context", context, "--file", str(path),
        )
        config = yaml.safe_load(path.read_text(encoding="utf-8"))
    connection_credentials(config)
    return config


def wait_deleted(load, request, description, timeout, state):
    deadline = time.monotonic() + timeout
    while True:
        remaining = load()
        if remaining is None:
            return
        owned(remaining, request, description)
        phase = state(remaining)
        if phase in ("Failed", "Canceled", "Cancelled"):
            error = remaining.get("properties", {}).get("error") or remaining.get("error") or {}
            fail(
                "deletion-failed",
                f"{description} deletion ended in {phase}: {json.dumps(error, separators=(',', ':'))}",
            )
        if time.monotonic() >= deadline:
            fail("deletion-timeout", f"{description} still exists after {timeout} seconds")
        time.sleep(5)


def delete(request):
    parameters, saved = settings(request, teardown=True)
    remote = deployment(parameters, saved)
    if remote and remote["properties"]["provisioningState"] in PENDING:
        fail("operation-conflict", "The owned Azure deployment is still running; retry deletion after it completes")
    current = cluster(parameters)
    if current:
        owned(current, request, "AKS cluster")
    eventing_group = parameters["eventingResourceGroup"]
    existing = group(parameters, eventing_group)
    principals = set(saved.get("managedPrincipals", []))
    assignments = set(saved.get("managedRoleAssignments", []))
    assignments.update(saved_publisher_assignments(parameters, saved))
    if existing:
        owned(existing, request, "Eventing resource group")
        identities = az("identity", "list", "--subscription", parameters["subscriptionId"], "--resource-group", eventing_group)
        principals.update(
            identity["principalId"] for identity in identities
            if identity.get("tags", {}).get(OWNER_TAG) == request["installationId"]
        )
    scope = resource_id(parameters)
    prefix = resource_id(parameters, "providers", "Microsoft.Authorization", "roleAssignments") + "/"
    for principal in principals:
        if not guid(principal):
            fail("invalid-state", "Managed identity principal is invalid")
        listed_assignments = az(
            "role", "assignment", "list", "--subscription", parameters["subscriptionId"],
            "--scope", scope, "--fill-principal-name", "false",
        )
        for role in listed_assignments:
            if role.get("principalId", "").lower() != principal.lower() or role.get("scope", "").lower() != scope.lower():
                continue
            assignment = role["id"]
            if not assignment.lower().startswith(prefix.lower()) or not guid(assignment[len(prefix):]):
                fail("not-owned", "Publisher role assignment is outside this installation's authorized scope")
            assignments.add(assignment)
    if any(
        not isinstance(assignment, str)
        or not assignment.lower().startswith(prefix.lower())
        or not guid(assignment[len(prefix):])
        for assignment in assignments
    ):
        fail("invalid-state", "Saved publisher role assignment is outside this installation's authorized scope")
    saved.update(
        phase="deleting-eventing",
        managedPrincipals=sorted(principals),
        managedRoleAssignments=sorted(assignments),
    )
    save(saved)
    if request["config"].get("deleteCluster") and current:
        saved["phase"] = "deleting-cluster"
        save(saved)
        if current.get("provisioningState") != "Deleting":
            az(
                "aks", "delete", "--subscription", parameters["subscriptionId"],
                "--resource-group", parameters["resourceGroupName"], "--name", parameters["clusterName"],
                "--yes", "--no-wait",
            )
        wait_deleted(
            lambda: cluster(parameters),
            request,
            "AKS cluster",
            3600,
            lambda resource: resource.get("provisioningState"),
        )
    for assignment in sorted(assignments):
        az("role", "assignment", "delete", "--subscription", parameters["subscriptionId"], "--ids", assignment)
    if existing:
        if existing.get("properties", {}).get("provisioningState") != "Deleting":
            az("group", "delete", "--subscription", parameters["subscriptionId"], "--name", eventing_group, "--yes", "--no-wait")
        wait_deleted(
            lambda: group(parameters, eventing_group),
            request,
            "Eventing resource group",
            1800,
            lambda resource: resource.get("properties", {}).get("provisioningState"),
        )
    cluster_deleted = request["config"].get("deleteCluster") is True
    saved["phase"] = "deleted" if cluster_deleted else "retained"
    if cluster_deleted:
        saved.pop("outputs", None)
    saved["managedPrincipals"] = []
    saved["managedRoleAssignments"] = []
    save(saved)
    return {"deleted": True, "clusterRetained": not request["config"].get("deleteCluster", False)}








def eventing_inventory(parameters, request, outputs):
    expected = deployment_scope(parameters, request)
    for key, value in expected.items():
        if outputs.get(key) != value:
            fail("invalid-output", f"Azure deployment output {key} does not match this installation")
    if not re.fullmatch(r"[0-9a-fA-F]{64}", outputs.get("sourceDigest", "")):
        fail("invalid-output", "Azure deployment outputs are not bound to customer source")
    inventory = outputs.get("eventing")
    if not isinstance(inventory, dict):
        fail("invalid-output", "Azure Eventing inventory must be an object")
    if not inventory:
        return None
    group_name = parameters["eventingResourceGroup"]
    if inventory.get("resourceGroup") != group_name or inventory.get("publisherResourceGroup", "").lower() != parameters["resourceGroupName"].lower():
        fail("invalid-output", "Azure Eventing has a different dedicated or publisher resource group")
    scope = resource_id(parameters, group=group_name)
    if inventory.get("resourceGroupId", "").lower() != scope.lower():
        fail("invalid-output", "Azure Eventing returned another resource group scope")
    for key, kind in (
        ("publicIpId", "Microsoft.Network/publicIPAddresses"),
        ("controllerIdentityId", "Microsoft.ManagedIdentity/userAssignedIdentities"),
        ("fiberdIdentityId", "Microsoft.ManagedIdentity/userAssignedIdentities"),
        ("registryId", "Microsoft.ContainerRegistry/registries"),
    ):
        prefix = scope + "/providers/" + kind + "/"
        value = inventory.get(key, "")
        if not value.lower().startswith(prefix.lower()) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", value[len(prefix):]):
            fail("invalid-output", f"Azure Eventing {key} is outside its dedicated scope")
    if inventory["publicIpId"].rsplit("/", 1)[-1] != inventory.get("publicIpName"):
        fail("invalid-output", "Azure Eventing public IP name does not match its resource ID")
    registry = inventory["registryId"].rsplit("/", 1)[-1]
    if not re.fullmatch(r"[A-Za-z0-9]{5,50}", registry) or inventory.get("registryLoginServer") != registry.lower() + ".azurecr.io":
        fail("invalid-output", "Azure Eventing registry hostname does not match its ID")
    webhook = inventory.get("webhookHost", "")
    if len(webhook) > 253 or not re.fullmatch(r"(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+azurefd\.net", webhook):
        fail("invalid-output", "Azure Eventing webhook must use an Azure Front Door hostname")
    for key in ("frontDoorId", "controllerPrincipalId", "controllerClientId", "fiberdClientId"):
        if not guid(inventory.get(key)):
            fail("invalid-output", f"Azure Eventing {key} must be a GUID")
    prefix = resource_id(parameters, "providers", "Microsoft.Authorization", "roleAssignments") + "/"
    roles = inventory.get("publisherRoleAssignments")
    if not isinstance(roles, list) or not roles or any(
        not isinstance(role, str) or not role.lower().startswith(prefix.lower()) or not guid(role[len(prefix):])
        for role in roles
    ):
        fail("invalid-output", "Azure Eventing publisher role assignment is outside its authorized scope")
    return inventory


def saved_publisher_assignments(parameters, saved):
    inventory = saved.get("outputs", {}).get("eventing", {})
    if not isinstance(inventory, dict):
        fail("invalid-state", "Saved Azure Eventing inventory must be an object")
    roles = inventory.get("publisherRoleAssignments", [])
    if not isinstance(roles, list):
        fail("invalid-state", "Saved Azure Eventing publisher role assignments must be an array")
    prefix = resource_id(parameters, "providers", "Microsoft.Authorization", "roleAssignments") + "/"
    if any(
        not isinstance(role, str)
        or not role.lower().startswith(prefix.lower())
        or not guid(role[len(prefix):])
        for role in roles
    ):
        fail("invalid-state", "Saved publisher role assignment is outside this installation's authorized scope")
    return roles


def runtime_images(request, eventing):
    config = request["config"]
    previous = {}
    event_grid_keys = ("eventGridControllerImage", "eventGridAdapterImage")
    if "nodeSetupImage" not in config or (eventing and any(key not in config for key in event_grid_keys)):
        previous = hyperlight.call("state.read", "runtime-images") or {}
        if not isinstance(previous, dict):
            fail("invalid-state", "Saved Azure runtime images must be an object")

    node_setup = config.get("nodeSetupImage", previous.get("nodeSetupImage"))
    if not digest_image(node_setup):
        fail("invalid-config", "Azure requires nodeSetupImage containing a prepublished image digest")

    images = {"nodeSetupImage": node_setup}
    if eventing:
        for key in event_grid_keys:
            image = config.get(key, previous.get(key))
            if not digest_image(image):
                component = "controller" if key == "eventGridControllerImage" else "adapter"
                fail("invalid-config", f"Event Grid requires {key} containing a prepublished {component} image digest")
            images[key] = image
    hyperlight.call("state.write", {"key": "runtime-images", "value": images})
    return images


def digest_image(image):
    return (
        isinstance(image, str) and not image.startswith("-")
        and not any(char.isspace() for char in image)
        and re.fullmatch(r".+@sha256:[0-9a-f]{64}", image) is not None
    )


def runtime_manifests(request, parameters, outputs, images):
    import yaml
    setup = yaml.safe_load((TEMPLATES / "aks-kvm-setup.yaml").read_text(encoding="utf-8"))
    container = setup["spec"]["template"]["spec"]["initContainers"][0]
    container["image"] = images["nodeSetupImage"]
    container["args"] = ["install"]
    setup["spec"]["template"].setdefault("metadata", {}).setdefault("annotations", {})[
        "lampyr.dev/node-preparation"
    ] = hashlib.sha256(f"{images['nodeSetupImage']}\0install".encode()).hexdigest()
    before = [setup]
    inventory = eventing_inventory(parameters, request, outputs)
    after = []
    metadata = {"subscriptionId": parameters["subscriptionId"], "resourceGroup": parameters["resourceGroupName"]}
    if inventory:
        metadata["eventing.azure.resources"] = json.dumps(inventory, separators=(",", ":"))
        bindings = dict(inventory, subscriptionId=parameters["subscriptionId"])
        after = EventGrid(TEMPLATES, fail).manifests(images, bindings)["items"]
    return {
        "options": {
            "httpsPort": 443, "metadata": metadata,
            # fiberd-k8s-controller refuses a grant whose Pod carries an
            # AppArmor annotation, so on AKS grant Pods run with its security
            # context, as on every other provider: privileged, and so
            # unconfined.
            "homePodMetadata": {"labels": {"azure.workload.identity/use": "true"}},
        },
        # Loading the KVM modules leaves nothing for a kept cluster to
        # undo, so removal needs no cleanup DaemonSet.
        "before": before, "after": after, "cleanup": [],
    }


def source_definitions():
    path = WORKSPACE / EVENT_SOURCES
    if path.is_symlink() or not path.is_file():
        fail("invalid-source", f"Azure workspace is missing {EVENT_SOURCES}")
    if path.stat().st_size > 1024 * 1024:
        fail("invalid-source", f"{EVENT_SOURCES} exceeds 1 MiB")
    try:
        sources = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail("invalid-source", f"Cannot read {EVENT_SOURCES}: {error}")
    validate_event_sources(sources)
    return sources


def validate_source_request(request):
    config = request.get("config")
    if not isinstance(config, dict):
        fail("invalid-input", "Event source request must be an object")
    unknown = config.keys() - {"name", "namespace", "service", "observations"}
    if unknown:
        fail("invalid-input", "Unknown event source request keys: " + ", ".join(sorted(map(str, unknown))))
    for key in ("name", "namespace", "service"):
        value = config.get(key)
        if not isinstance(value, str) or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", value):
            fail("invalid-input", f"Event source {key} must be a DNS label")
    observations = config.get("observations", [])
    if not isinstance(observations, list) or len(observations) > 32:
        fail("invalid-input", "Event source observations must be an array with at most 32 entries")
    for observation in observations:
        if not isinstance(observation, dict) or set(observation) != {"identity", "object"}:
            fail("invalid-input", "Invalid event source observation")
        if not isinstance(observation["identity"], dict):
            fail("invalid-input", "Invalid event source observation identity")


def source_identity(api_version, kind, namespace, name):
    return {
        "apiVersion": api_version,
        "kind": kind,
        "metadata": {"name": name, "namespace": namespace},
    }


def source_objects(request):
    return {
        observation["identity"].get("kind"): observation.get("object")
        for observation in request["config"].get("observations", [])
    }


def source_resource_id(definition, parameters):
    if "sourceResourceId" in definition:
        identifier = definition["sourceResourceId"]
    else:
        group_name = definition.get("resourceGroup", parameters["resourceGroupName"])
        identifier = (
            f"/subscriptions/{parameters['subscriptionId']}/resourceGroups/{group_name}"
            f"/providers/Microsoft.Storage/storageAccounts/{definition['storageAccount']}"
        )
    match = re.fullmatch(
        r"/subscriptions/([^/]+)/resourceGroups/([^/]+)/providers/"
        r"Microsoft\.Storage/storageAccounts/([^/]+)",
        identifier,
        re.IGNORECASE,
    )
    if (
        not match or match.group(1).lower() != parameters["subscriptionId"].lower()
        or match.group(2).lower() != parameters["resourceGroupName"].lower()
        or not re.fullmatch(r"[a-z0-9]{3,24}", match.group(3))
    ):
        fail("invalid-source", "Azure Event Grid source must identify a storage account in the installation resource group")
    return identifier


def source_create(request):
    saved = state()
    if not saved or not saved["parameters"].get("eventingEnabled"):
        fail(
            "eventing-required",
            "Azure Eventing is not enabled; set eventingEnabled to true in the "
            "customer Workspace and rerun cluster install --eventing",
        )
    config = request["config"]
    definition = source_definitions().get(config["name"])
    if definition is None:
        fail("unknown-source", f"Azure workspace has no eventSources.{config['name']} definition")
    parameters = saved["parameters"]
    identifier = source_resource_id(definition, parameters)
    current = source_objects(request).get("AzureEventGridSource")
    if current is not None and current.get("spec", {}).get("sourceResourceID") != identifier:
        fail("immutable-source", "sourceResourceId is immutable; delete and recreate the source")
    event_type = definition.get("eventTypes", ["Microsoft.Storage.BlobCreated"])[0]
    subscription = {"includedEventTypes": [event_type]}
    if "container" in definition:
        subscription["subjectBeginsWith"] = (
            f"/blobServices/default/containers/{definition['container']}/"
        )
    namespace = config["namespace"]
    name = config["name"]
    service = config["service"]
    broker = source_identity("eventing.knative.dev/v1", "Broker", namespace, "default")
    broker["spec"] = {}
    trigger = source_identity("eventing.knative.dev/v1", "Trigger", namespace, name)
    trigger["spec"] = {
        "broker": "default",
        "filter": {"attributes": {"type": event_type}},
        "subscriber": {
            "ref": {
                "apiVersion": "serving.knative.dev/v1",
                "kind": "Service",
                "name": service,
            },
            "uri": "/events/knative",
        },
    }
    source = source_identity("sources.lampyr.dev/v1alpha1", "AzureEventGridSource", namespace, name)
    source["spec"] = {
        "sourceResourceID": identifier,
        "eventSubscription": subscription,
        "sink": {
            "ref": {
                "apiVersion": "eventing.knative.dev/v1",
                "kind": "Broker",
                "name": "default",
            },
        },
    }
    return {
        "sharedManifests": [broker],
        "manifests": [trigger, source],
        "observations": [
            source_identity("eventing.knative.dev/v1", "Broker", namespace, "default"),
            source_identity("eventing.knative.dev/v1", "Trigger", namespace, name),
            source_identity("sources.lampyr.dev/v1alpha1", "AzureEventGridSource", namespace, name),
        ],
        "timeoutSeconds": 2100,
    }


def ready_condition(resource):
    if not isinstance(resource, dict):
        return None
    generation = resource.get("metadata", {}).get("generation")
    status = resource.get("status", {})
    if status.get("observedGeneration") != generation:
        return None
    for condition in status.get("conditions", []):
        if condition.get("type") == "Ready" and condition.get("observedGeneration", generation) == generation:
            return condition
    return None


def source_status(request):
    objects = source_objects(request)
    for kind in ("Broker", "Trigger", "AzureEventGridSource"):
        resource = objects.get(kind)
        if resource is None:
            return {
                "ready": False,
                "reason": "NotFound",
                "message": f"Waiting for {kind}.",
                "terminal": False,
            }
        condition = ready_condition(resource)
        if condition is None:
            return {
                "ready": False,
                "reason": "Reconciling",
                "message": f"Waiting for {kind} to observe its latest generation.",
                "terminal": False,
            }
        if condition.get("status") != "True":
            reason = str(condition.get("reason", "NotReady"))
            return {
                "ready": False,
                "reason": reason,
                "message": str(condition.get("message", f"{kind} is not ready.")),
                "terminal": reason in ("InvalidSpec", "OwnershipConflict"),
            }
    return {
        "ready": True,
        "reason": "Ready",
        "message": "Azure Event Grid source is ready.",
        "terminal": False,
    }


def source_delete(request):
    config = request["config"]
    return {
        "manifests": [
            source_identity("sources.lampyr.dev/v1alpha1", "AzureEventGridSource", config["namespace"], config["name"]),
            source_identity("eventing.knative.dev/v1", "Trigger", config["namespace"], config["name"]),
        ],
    }

def invoke(request):
    try:
        if request["protocolVersion"] != 1:
            fail("protocol-version", "Unsupported provider protocol")
        operation = request["operation"]
        if operation == "describe":
            result = json.loads(Path("/provider/manifest.json").read_text(encoding="utf-8"))
        elif operation in ("source-create", "source-status", "source-delete"):
            validate_source_request(request)
            result = {
                "source-create": source_create,
                "source-status": source_status,
                "source-delete": source_delete,
            }[operation](request)
        else:
            validate_config(request)
            result = {
                "init": initialize, "apply": apply, "inspect": inspect,
                "connect": connect, "delete": delete,
            }[operation](request)
        return {"protocolVersion": 1, "status": "ok", "result": result}
    except ProviderError as error:
        return {"protocolVersion": 1, "status": "error", "code": error.code, "message": str(error)}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"protocolVersion": 1, "status": "error", "code": "provider-failed", "message": str(error)}
    except (KeyError, TypeError, ValueError) as error:
        return {"protocolVersion": 1, "status": "error", "code": "invalid-input", "message": f"Invalid Azure request or response: {error}"}
