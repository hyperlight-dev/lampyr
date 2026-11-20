"""Azure DNS ingress validation, reconciliation, and cleanup."""

import hashlib
import ipaddress
import json
from pathlib import Path
import re
import urllib.parse


DNS_ZONE = re.compile(
    r"/subscriptions/([^/]+)/resourceGroups/([^/]+)/providers/"
    r"Microsoft\.Network/dnsZones/([^/]+)",
    re.IGNORECASE,
)


class Ingress:
    def __init__(self, fail, owner_tag, arm):
        self.fail = fail
        self.owner_tag = owner_tag
        self.arm = arm

    @staticmethod
    def guid(value):
        return isinstance(value, str) and re.fullmatch(
            r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", value,
        ) is not None

    def runtime_credential(self, request, zone, inventory):
        installation_id = request["installationId"]
        client_id = inventory["dnsIdentityClientId"]
        policy = "lampyr-azure-dns-issuer-boundary"
        message = (
            "Azure DNS Issuers that use Lampyr-managed ambient credentials "
            "are restricted to kourier-system"
        )
        azure_dns = {
            "subscriptionID": zone["subscriptionId"],
            "resourceGroupName": zone["resourceGroup"],
            "hostedZoneName": zone["name"],
            "environment": "AzurePublicCloud",
            "managedIdentity": {"clientID": client_id},
        }
        annotations = {
            "lampyr.dev/installation-id": installation_id,
            "lampyr.dev/credential-boundary": "true",
        }
        expression = (
            "object.metadata.namespace == 'kourier-system' || "
            "(request.resource.resource == 'issuers' && "
            "(!has(object.spec.acme) || !has(object.spec.acme.solvers) || "
            "!object.spec.acme.solvers.exists(s, has(s.dns01) && "
            "has(s.dns01.azureDNS)))) || "
            "(request.resource.resource == 'challenges' && "
            "((has(object.spec.issuerRef) && has(object.spec.issuerRef.kind) && "
            "object.spec.issuerRef.kind != 'Issuer') || "
            "!has(object.spec.solver) || !has(object.spec.solver.dns01) || "
            "!has(object.spec.solver.dns01.azureDNS)))"
        )
        issuer_probe = {
            "apiVersion": "cert-manager.io/v1",
            "kind": "Issuer",
            "metadata": {"name": "lampyr-credential-boundary-probe", "namespace": "default"},
            "spec": {"acme": {
                "email": "probe@example.com",
                "server": "https://acme.invalid/directory",
                "privateKeySecretRef": {"name": "probe"},
                "solvers": [{"dns01": {"azureDNS": azure_dns}}],
            }},
        }
        challenge_probe = {
            "apiVersion": "acme.cert-manager.io/v1",
            "kind": "Challenge",
            "metadata": {"name": "lampyr-credential-boundary-probe", "namespace": "default"},
            "spec": {
                "authorizationURL": "https://acme.invalid/authorization",
                "dnsName": "example.com",
                "issuerRef": {"group": "cert-manager.io", "kind": "Issuer", "name": "probe"},
                "key": "probe",
                "solver": {"dns01": {"azureDNS": azure_dns}},
                "token": "probe",
                "type": "DNS-01",
                "url": "https://acme.invalid/challenge",
                "wildcard": False,
            },
        }
        return {
            "solver": {"dns01": {"azureDNS": azure_dns}},
            "credential": {
                "serviceAccountPatch": {"metadata": {"annotations": {
                    "azure.workload.identity/client-id": client_id,
                }}},
                "serviceAccountCleanupPatch": {"metadata": {"annotations": {
                    "azure.workload.identity/client-id": None,
                }}},
                "deploymentPatch": {"spec": {"template": {"metadata": {
                    "annotations": {
                        "lampyr.dev/azure-workload-identity-client-id": client_id,
                    },
                    "labels": {"azure.workload.identity/use": "true"},
                }}}},
                "deploymentCleanupPatch": {"spec": {"template": {"metadata": {
                    "annotations": {
                        "lampyr.dev/azure-workload-identity-client-id": None,
                    },
                    "labels": {"azure.workload.identity/use": None},
                }}}},
                "controllerArgs": ["--issuer-ambient-credentials=true"],
                "controllerArgPrefixes": ["--issuer-ambient-credentials"],
                "boundaryMessage": message,
                "boundary": {"apiVersion": "v1", "kind": "List", "items": [
                    {
                        "apiVersion": "admissionregistration.k8s.io/v1",
                        "kind": "ValidatingAdmissionPolicy",
                        "metadata": {"name": policy, "annotations": annotations},
                        "spec": {
                            "failurePolicy": "Fail",
                            "matchConstraints": {"resourceRules": [
                                {"apiGroups": ["cert-manager.io"], "apiVersions": ["v1"],
                                 "operations": ["CREATE", "UPDATE"], "resources": ["issuers"]},
                                {"apiGroups": ["acme.cert-manager.io"], "apiVersions": ["v1"],
                                 "operations": ["CREATE", "UPDATE"], "resources": ["challenges"]},
                            ]},
                            "validations": [{"expression": expression, "message": message}],
                        },
                    },
                    {
                        "apiVersion": "admissionregistration.k8s.io/v1",
                        "kind": "ValidatingAdmissionPolicyBinding",
                        "metadata": {"name": policy + "-binding", "annotations": annotations},
                        "spec": {"policyName": policy, "validationActions": ["Deny"]},
                    },
                ]},
                "probes": [issuer_probe, challenge_probe],
                "audits": [
                    {"resource": "issuers.cert-manager.io", "kind": "Issuer",
                     "allowedNamespace": "kourier-system",
                     "fieldPath": ["spec", "acme", "solvers", "*", "dns01", "azureDNS"]},
                    {"resource": "challenges.acme.cert-manager.io", "kind": "Challenge",
                     "allowedNamespace": "kourier-system",
                     "fieldPath": ["spec", "solver", "dns01", "azureDNS"],
                     "issuerKind": "Issuer"},
                ],
            },
        }

    def zone(self, resource_id):
        match = DNS_ZONE.fullmatch(resource_id) if isinstance(resource_id, str) else None
        if not match or not self.guid(match.group(1)):
            self.fail(
                "invalid-config",
                "ingress.azureDnsZoneResourceId must be an Azure DNS zone resource ID",
            )
        subscription, group_name, zone_name = match.groups()
        if (
            not re.fullmatch(r"[A-Za-z0-9_().-]{1,90}", group_name)
            or group_name.endswith(".")
            or not re.fullmatch(
                r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
                r"(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+",
                zone_name,
            )
        ):
            self.fail(
                "invalid-config",
                "ingress.azureDnsZoneResourceId is not a valid public Azure DNS zone",
            )
        return {
            "id": resource_id,
            "subscriptionId": subscription,
            "resourceGroup": group_name,
            "name": zone_name,
        }

    def config(self, config):
        ingress = config.get("ingress", {})
        if not isinstance(ingress, dict):
            self.fail("invalid-config", "ingress must be an object")
        unknown = ingress.keys() - {"azureDnsZoneResourceId", "tls"}
        if unknown:
            self.fail(
                "invalid-config",
                "Unknown ingress keys: " + ", ".join(sorted(map(str, unknown))),
            )
        tls = ingress.get("tls", {"mode": "selfSigned"})
        if not isinstance(tls, dict):
            self.fail("invalid-config", "ingress.tls must be an object")
        tls = dict(tls)
        mode = tls.get("mode", "selfSigned")
        tls["mode"] = mode
        allowed = {
            "selfSigned": {"mode"},
            "acme": {"mode", "email", "issuer"},
            "existingSecret": {"mode", "secretName", "secretNamespace"},
        }
        if mode not in allowed:
            self.fail(
                "invalid-config",
                "ingress.tls.mode must be selfSigned, acme, or existingSecret",
            )
        unknown = tls.keys() - allowed[mode]
        if unknown:
            self.fail(
                "invalid-config",
                "Unknown ingress.tls keys: " + ", ".join(sorted(map(str, unknown))),
            )
        zone = (
            self.zone(ingress["azureDnsZoneResourceId"])
            if "azureDnsZoneResourceId" in ingress
            else None
        )
        if mode == "acme":
            email = tls.get("email")
            if (
                not isinstance(email, str)
                or len(email) > 254
                or not re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", email)
            ):
                self.fail(
                    "invalid-config",
                    "ingress.tls.email must be a valid ACME contact email",
                )
            if zone is None:
                self.fail(
                    "invalid-config",
                    "ingress.tls.mode acme requires ingress.azureDnsZoneResourceId",
                )
            issuer = tls.get("issuer", "letsencrypt-prod")
            if not isinstance(issuer, str) or not re.fullmatch(
                r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", issuer,
            ):
                self.fail("invalid-config", "ingress.tls.issuer must be a DNS label")
        elif mode == "existingSecret":
            for key, default in (
                ("secretName", "lampyr-kourier-tls"),
                ("secretNamespace", "default"),
            ):
                value = tls.get(key, default)
                if not isinstance(value, str) or not re.fullmatch(
                    r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", value,
                ):
                    self.fail(
                        "invalid-config",
                        f"ingress.tls.{key} must be a DNS label",
                    )
            if tls.get("secretNamespace", "default") in {
                "cert-manager",
                "fiberd-system",
                "hyperlight-system",
                "knative-eventing",
                "knative-serving",
                "kourier-system",
                "lampyr-system",
            }:
                self.fail(
                    "invalid-config",
                    "ingress.tls.secretNamespace must be outside Lampyr-owned namespaces",
                )
        elif zone is not None:
            self.fail(
                "invalid-config",
                "Azure DNS requires acme or existingSecret TLS mode",
            )
        domain = config.get("domain", "localhost")
        if zone is not None and not (
            domain == zone["name"] or domain.endswith("." + zone["name"])
        ):
            self.fail(
                "invalid-config",
                "domain must be inside ingress.azureDnsZoneResourceId",
            )
        return ingress, tls, zone

    def validate_zone_scope(self, zone, parameters, code):
        if (
            zone["subscriptionId"].lower()
            == parameters["subscriptionId"].lower()
            and zone["resourceGroup"].lower()
            in {
                parameters["eventingResourceGroup"].lower(),
                parameters["ingressResourceGroup"].lower(),
            }
        ):
            self.fail(
                code,
                "Azure DNS zone must be outside Lampyr's dedicated resource groups",
            )

    def validate_scope(self, config, parameters):
        _, _, zone = self.config(config)
        if zone is not None:
            self.validate_zone_scope(zone, parameters, "invalid-config")

    def bindings(self, config):
        _, tls, zone = self.config(config)
        if zone is None:
            return {
                "ingressDnsEnabled": False,
                "ingressAcmeEnabled": False,
                "ingressWorkloadIdentityEnabled": False,
                "ingressDnsSubscriptionId": "",
                "ingressDnsResourceGroup": "",
                "ingressDnsZone": "",
                "ingressDnsRecord": "",
            }
        domain = config["domain"]
        relative = domain[: -(len(zone["name"]) + 1)] if domain != zone["name"] else ""
        acme = tls.get("mode", "selfSigned") == "acme"
        return {
            "ingressDnsEnabled": True,
            "ingressAcmeEnabled": acme,
            "ingressWorkloadIdentityEnabled": acme,
            "ingressDnsSubscriptionId": zone["subscriptionId"],
            "ingressDnsResourceGroup": zone["resourceGroup"],
            "ingressDnsZone": zone["name"],
            "ingressDnsRecord": relative,
        }

    @staticmethod
    def names(bindings):
        relative = bindings["ingressDnsRecord"]
        return [
            "*" if not relative else f"*.{relative}",
            "*.default" if not relative else f"*.default.{relative}",
        ]

    def ensure_records_available(self, config, installation_id, az):
        bindings = self.bindings(config)
        if not bindings["ingressDnsEnabled"]:
            return
        for name in self.names(bindings):
            current = az(
                "network",
                "dns",
                "record-set",
                "a",
                "show",
                "--subscription",
                bindings["ingressDnsSubscriptionId"],
                "--resource-group",
                bindings["ingressDnsResourceGroup"],
                "--zone-name",
                bindings["ingressDnsZone"],
                "--name",
                name,
                missing=True,
            )
            if (
                current is not None
                and current.get("metadata", {}).get(self.owner_tag) != installation_id
            ):
                self.fail(
                    "not-owned",
                    f"Azure DNS record {name}.{bindings['ingressDnsZone']} already exists and is not owned by this installation",
                )

    def claim(self, parameters, request, resource_id):
        bindings = self.bindings(request["config"])
        if not bindings["ingressDnsEnabled"]:
            return None
        zone = self.zone(request["config"]["ingress"]["azureDnsZoneResourceId"])
        group_id = resource_id(parameters, group=parameters["ingressResourceGroup"])
        return {
            "resourceGroup": parameters["ingressResourceGroup"],
            "resourceGroupId": group_id,
            "dnsZoneId": zone["id"],
            "dnsRecordSetIds": [
                f"{zone['id']}/A/{name}" for name in self.names(bindings)
            ],
        }

    def reconcile_records(self, request, inventory, az, provider_error):
        address = str(ipaddress.IPv4Address(inventory["publicIpAddress"]))
        zone = self.zone(inventory["dnsZoneId"])
        prefix = zone["id"] + "/A/"
        for record_id in inventory["dnsRecordSetIds"]:
            name = record_id[len(prefix) :]
            current = az(
                "network",
                "dns",
                "record-set",
                "a",
                "show",
                "--subscription",
                zone["subscriptionId"],
                "--resource-group",
                zone["resourceGroup"],
                "--zone-name",
                zone["name"],
                "--name",
                name,
                missing=True,
            )
            if current is None:
                condition = "If-None-Match=*"
            else:
                if (
                    current.get("metadata", {}).get(self.owner_tag)
                    != request["installationId"]
                ):
                    self.fail(
                        "not-owned",
                        f"Azure DNS record {name}.{zone['name']} is not owned by this installation",
                    )
                etag = current.get("etag")
                if not isinstance(etag, str) or not etag:
                    self.fail(
                        "invalid-output",
                        "Azure DNS record has no concurrency token",
                    )
                condition = f"If-Match={etag}"
            body = json.dumps(
                {
                    "properties": {
                        "metadata": {
                            self.owner_tag: request["installationId"],
                        },
                        "TTL": 60,
                        "ARecords": [{"ipv4Address": address}],
                    },
                },
                separators=(",", ":"),
            )
            url = (
                self.arm
                + urllib.parse.quote(record_id, safe="/")
                + "?api-version=2018-05-01"
            )
            try:
                result = az(
                    "rest",
                    "--method",
                    "put",
                    "--url",
                    url,
                    "--headers",
                    condition,
                    "--body",
                    body,
                )
            except provider_error as error:
                if error.code == "azure-command" and "Precondition" in str(error):
                    self.fail(
                        "operation-conflict",
                        f"Azure DNS record {name}.{zone['name']} changed concurrently",
                    )
                raise
            if (
                result.get("properties", {})
                .get("metadata", {})
                .get(self.owner_tag)
                != request["installationId"]
                or result.get("properties", {}).get("ARecords")
                != [{"ipv4Address": address}]
            ):
                self.fail(
                    "invalid-output",
                    "Azure DNS record reconciliation returned unexpected state",
                )

    def deployment_digest(self, source, request):
        bindings = self.bindings(request["config"])
        if not bindings["ingressDnsEnabled"]:
            return source
        content = json.dumps(bindings, sort_keys=True, separators=(",", ":"))
        return hashlib.sha256(f"{source}\0{content}".encode()).hexdigest()

    @staticmethod
    def source_supports(source):
        try:
            text = (Path(source) / "main.bicep").read_text(encoding="utf-8")
        except FileNotFoundError:
            return False
        return "param ingressDnsEnabled" in text

    def parameter_args(self, config, saved, parameters, current_cluster=None):
        bindings = self.bindings(config)
        if any(
            isinstance(inventory, dict) and inventory.get("dnsIdentityId")
            for inventory in saved.get("managedIngress", [])
        ):
            bindings["ingressWorkloadIdentityEnabled"] = True
        current_cluster = current_cluster or {}
        oidc = current_cluster.get("oidcIssuerProfile", {})
        workload_identity = current_cluster.get(
            "securityProfile", {},
        ).get("workloadIdentity", {})
        bindings["clusterOidcIssuerEnabled"] = bool(
            oidc.get("enabled") or oidc.get("issuerUrl")
        )
        bindings["clusterWorkloadIdentityEnabled"] = bool(
            workload_identity.get("enabled")
        )
        return [
            f"ingressResourceGroup={parameters['ingressResourceGroup']}",
            *[
                f"{name}={'true' if value is True else 'false' if value is False else value}"
                for name, value in bindings.items()
            ],
        ]

    def inventory(
        self,
        parameters,
        request,
        outputs,
        resource_id,
        group,
        owned,
        cluster_resource=None,
    ):
        inventory = outputs.get("ingress", {})
        bindings = self.bindings(request["config"])
        if (
            not isinstance(inventory, dict)
            or bool(inventory) != bindings["ingressDnsEnabled"]
        ):
            self.fail(
                "invalid-output",
                "Azure ingress outputs do not match the requested DNS configuration",
            )
        if not inventory:
            return None
        group_name = parameters["ingressResourceGroup"]
        scope = resource_id(parameters, group=group_name)
        if (
            inventory.get("resourceGroup") != group_name
            or inventory.get("resourceGroupId", "").lower() != scope.lower()
        ):
            self.fail(
                "invalid-output",
                "Azure ingress resources use another resource group",
            )
        if cluster_resource is not None:
            owned(group(parameters, group_name) or {}, request, "Ingress resource group")
        public_ip_prefix = (
            scope + "/providers/Microsoft.Network/publicIPAddresses/"
        )
        public_ip_id = inventory.get("publicIpId", "")
        if (
            not public_ip_id.lower().startswith(public_ip_prefix.lower())
            or public_ip_id.rsplit("/", 1)[-1] != inventory.get("publicIpName")
            or not re.fullmatch(
                r"[A-Za-z0-9_.-]{1,80}",
                inventory.get("publicIpName", ""),
            )
        ):
            self.fail(
                "invalid-output",
                "Azure ingress public IP is outside the installation resource group",
            )
        try:
            ipaddress.IPv4Address(inventory.get("publicIpAddress", ""))
        except ipaddress.AddressValueError:
            self.fail("invalid-output", "Azure ingress public IP address is invalid")
        zone = self.zone(request["config"]["ingress"]["azureDnsZoneResourceId"])
        if inventory.get("dnsZoneId", "").lower() != zone["id"].lower():
            self.fail(
                "invalid-output",
                "Azure ingress DNS records use another DNS zone",
            )
        expected_names = set(self.names(bindings))
        records = inventory.get("dnsRecordSetIds")
        record_prefix = zone["id"] + "/A/"
        if (
            not isinstance(records, list)
            or len(records) != 2
            or {
                record[len(record_prefix) :]
                for record in records
                if isinstance(record, str)
                and record.lower().startswith(record_prefix.lower())
            }
            != expected_names
        ):
            self.fail(
                "invalid-output",
                "Azure ingress DNS record inventory is outside its authorized zone",
            )
        role_suffix = "/providers/Microsoft.Authorization/roleAssignments/"
        public_role = inventory.get("publicIpRoleAssignmentId", "")
        if (
            not public_role.lower().startswith((scope + role_suffix).lower())
            or not self.guid(public_role.rsplit("/", 1)[-1])
            or not self.guid(inventory.get("clusterPrincipalId"))
            or (
                cluster_resource is not None
                and inventory["clusterPrincipalId"].lower()
                != cluster_resource.get("identity", {})
                .get("principalId", "")
                .lower()
            )
        ):
            self.fail(
                "invalid-output",
                "Azure ingress public IP role assignment is outside its authorized scope",
            )
        dns_role = inventory.get("dnsRoleAssignmentId", "")
        if bindings["ingressAcmeEnabled"]:
            identity_prefix = (
                scope
                + "/providers/Microsoft.ManagedIdentity/userAssignedIdentities/"
            )
            identity = inventory.get("dnsIdentityId", "")
            if (
                not identity.lower().startswith(identity_prefix.lower())
                or not re.fullmatch(
                    r"[A-Za-z0-9_.-]{1,128}",
                    identity[len(identity_prefix) :],
                )
                or not self.guid(inventory.get("dnsIdentityClientId"))
                or not self.guid(inventory.get("dnsIdentityPrincipalId"))
                or not dns_role.lower().startswith(
                    (zone["id"] + role_suffix).lower()
                )
                or not self.guid(dns_role.rsplit("/", 1)[-1])
            ):
                self.fail(
                    "invalid-output",
                    "Azure ingress DNS identity is outside its authorized scope",
                )
        elif any(
            inventory.get(key)
            for key in (
                "dnsIdentityId",
                "dnsIdentityClientId",
                "dnsIdentityPrincipalId",
                "dnsRoleAssignmentId",
            )
        ):
            self.fail(
                "invalid-output",
                "Azure ingress returned a DNS identity for non-ACME TLS",
            )
        return inventory

    def saved_inventories(self, parameters, saved, resource_id, provider_error):
        inventories = saved.get("managedIngress", [])
        if not isinstance(inventories, list):
            self.fail(
                "invalid-state",
                "Saved Azure ingress inventory must be an array",
            )
        scope = resource_id(
            parameters,
            group=parameters["ingressResourceGroup"],
        )
        resources = []
        for inventory in inventories:
            if not isinstance(inventory, dict):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress inventory entry must be an object",
                )
            public_ip = inventory.get("publicIpId", "")
            if (
                inventory.get("resourceGroup")
                != parameters["ingressResourceGroup"]
                or inventory.get("resourceGroupId", "").lower() != scope.lower()
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress resource group is outside its authorized scope",
                )
            if not public_ip.lower().startswith(
                (
                    scope
                    + "/providers/Microsoft.Network/publicIPAddresses/"
                ).lower()
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress public IP is outside its authorized scope",
                )
            zone_id = inventory.get("dnsZoneId", "")
            try:
                self.zone(zone_id)
            except provider_error as error:
                self.fail("invalid-state", str(error))
            record_prefix = zone_id + "/A/"
            records = inventory.get("dnsRecordSetIds", [])
            if not isinstance(records, list) or any(
                not isinstance(record, str)
                or not record.lower().startswith(record_prefix.lower())
                for record in records
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress DNS records are outside their authorized zone",
                )
            role_suffix = (
                "/providers/Microsoft.Authorization/roleAssignments/"
            )
            roles = [
                (inventory.get("publicIpRoleAssignmentId", ""), scope),
                (inventory.get("dnsRoleAssignmentId", ""), zone_id),
            ]
            if any(
                role
                and (
                    not role.lower().startswith(
                        (role_scope + role_suffix).lower()
                    )
                    or not self.guid(role.rsplit("/", 1)[-1])
                )
                for role, role_scope in roles
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress role assignment is outside its authorized scope",
                )
            identity = inventory.get("dnsIdentityId", "")
            if not self.guid(inventory.get("clusterPrincipalId")) or (
                inventory.get("dnsRoleAssignmentId")
                and not self.guid(inventory.get("dnsIdentityPrincipalId"))
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress identity principal is invalid",
                )
            if identity and not identity.lower().startswith(
                (
                    scope
                    + "/providers/Microsoft.ManagedIdentity/userAssignedIdentities/"
                ).lower()
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress identity is outside its authorized scope",
                )
            resources.append(inventory)
        return resources

    def saved_claims(self, parameters, saved, resource_id, provider_error):
        claims = saved.get("managedIngressClaims", [])
        if not isinstance(claims, list):
            self.fail(
                "invalid-state",
                "Saved Azure ingress claims must be an array",
            )
        group_id = resource_id(
            parameters,
            group=parameters["ingressResourceGroup"],
        )
        validated = []
        for claim in claims:
            if (
                not isinstance(claim, dict)
                or claim.get("resourceGroup")
                != parameters["ingressResourceGroup"]
                or claim.get("resourceGroupId", "").lower()
                != group_id.lower()
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress claim has another resource group",
                )
            zone_id = claim.get("dnsZoneId", "")
            try:
                self.zone(zone_id)
            except provider_error as error:
                self.fail("invalid-state", str(error))
            records = claim.get("dnsRecordSetIds")
            if (
                not isinstance(records, list)
                or len(records) != 2
                or any(
                    not isinstance(record, str)
                    or not record.lower().startswith(
                        (zone_id + "/A/").lower()
                    )
                    for record in records
                )
            ):
                self.fail(
                    "invalid-state",
                    "Saved Azure ingress claim has invalid DNS records",
                )
            validated.append(claim)
        return validated

    def delete_record(self, record, request, az, provider_error):
        current = az("resource", "show", "--ids", record, missing=True)
        if current is None:
            return
        if (
            current.get("properties", {})
            .get("metadata", {})
            .get(self.owner_tag)
            != request["installationId"]
        ):
            self.fail(
                "not-owned",
                "Azure ingress DNS record is not owned by this Lampyr installation",
            )
        etag = current.get("etag")
        if not isinstance(etag, str) or not etag:
            self.fail(
                "invalid-output",
                "Azure ingress DNS record has no concurrency token",
            )
        url = (
            self.arm
            + urllib.parse.quote(record, safe="/")
            + "?api-version=2018-05-01"
        )
        try:
            az(
                "rest",
                "--method",
                "delete",
                "--url",
                url,
                "--headers",
                f"If-Match={etag}",
            )
        except provider_error as error:
            if error.code == "azure-command" and "Precondition" in str(error):
                self.fail(
                    "operation-conflict",
                    "Azure ingress DNS record changed during cleanup",
                )
            raise

    def delete_role(self, inventory, role, az):
        expected_scope = (
            inventory["dnsZoneId"]
            if role == inventory.get("dnsRoleAssignmentId")
            else inventory["resourceGroupId"]
        )
        matches = [
            assignment
            for assignment in az(
                "role",
                "assignment",
                "list",
                "--scope",
                expected_scope,
                "--fill-principal-name",
                "false",
            )
            if assignment.get("id", "").lower() == role.lower()
        ]
        if not matches:
            return
        if len(matches) != 1:
            self.fail(
                "invalid-state",
                "Azure returned duplicate ingress role assignments",
            )
        current = matches[0]
        if role == inventory.get("dnsRoleAssignmentId"):
            expected_principal = inventory["dnsIdentityPrincipalId"]
            expected_role = "befefa01-2a29-4197-83a8-272ff33ce314"
        else:
            expected_principal = inventory["clusterPrincipalId"]
            expected_role = "4d97b98b-1d4f-4787-a291-c67834d212e7"
        if (
            current.get("scope", "").lower() != expected_scope.lower()
            or not current.get("roleDefinitionId", "")
            .lower()
            .endswith("/" + expected_role)
            or current.get("principalId", "").lower()
            != expected_principal.lower()
        ):
            self.fail(
                "not-owned",
                "Azure ingress role assignment is not owned by this Lampyr installation",
            )
        az("role", "assignment", "delete", "--ids", role)

    def retire(
        self,
        parameters,
        request,
        saved,
        current,
        az,
        resource_id,
        group,
        owned,
        provider_error,
    ):
        inventories = self.saved_inventories(
            parameters, saved, resource_id, provider_error,
        )
        claims = self.saved_claims(
            parameters, saved, resource_id, provider_error,
        )
        current = current or {}
        current_records = {
            record.lower() for record in current.get("dnsRecordSetIds", [])
        }
        current_role = (current.get("dnsRoleAssignmentId") or "").lower()
        for inventory in inventories:
            for record in inventory["dnsRecordSetIds"]:
                if record.lower() not in current_records:
                    self.delete_record(record, request, az, provider_error)
            role = inventory.get("dnsRoleAssignmentId")
            if role and role.lower() != current_role:
                self.delete_role(inventory, role, az)
        for claim in claims:
            for record in claim["dnsRecordSetIds"]:
                if record.lower() not in current_records:
                    self.delete_record(record, request, az, provider_error)
        self.delete_claim_roles(
            parameters,
            request,
            [
                claim
                for claim in claims
                if claim["dnsZoneId"].lower()
                != current.get("dnsZoneId", "").lower()
            ],
            az,
            group,
            owned,
        )
        saved["managedIngress"] = [current] if current else []
        saved["managedIngressClaims"] = []

    def finalize(
        self,
        parameters,
        request,
        saved,
        az,
        resource_id,
        group,
        owned,
        provider_error,
    ):
        retiring = saved.get("retiringIngress", [])
        if not isinstance(retiring, list):
            self.fail(
                "invalid-state",
                "Saved retiring Azure ingress inventory must be an array",
            )
        if not retiring:
            return False
        pending = {"managedIngress": retiring}
        current = saved.get("managedIngress", [])
        current = current[0] if isinstance(current, list) and len(current) == 1 else {}
        self.retire(
            parameters,
            request,
            pending,
            current,
            az,
            resource_id,
            group,
            owned,
            provider_error,
        )
        saved["retiringIngress"] = []
        return True

    def record_transition(self, saved, current):
        retiring = saved.get("retiringIngress", [])
        if not isinstance(retiring, list):
            self.fail(
                "invalid-state",
                "Saved retiring Azure ingress inventory must be an array",
            )
        for previous in saved.get("managedIngress", []):
            if previous != current and previous not in retiring:
                retiring.append(previous)
        saved["retiringIngress"] = retiring
        saved["managedIngress"] = [current] if current else []

    def delete_claim_roles(
        self,
        parameters,
        request,
        resources,
        az,
        group,
        owned,
    ):
        ingress_group = group(parameters, parameters["ingressResourceGroup"])
        if not ingress_group:
            return None
        owned(ingress_group, request, "Ingress resource group")
        identities = az(
            "identity",
            "list",
            "--subscription",
            parameters["subscriptionId"],
            "--resource-group",
            parameters["ingressResourceGroup"],
        )
        principals = {
            identity["principalId"].lower()
            for identity in identities
            if identity.get("tags", {}).get(self.owner_tag)
            == request["installationId"]
            and self.guid(identity.get("principalId"))
        }
        for zone_id in {item["dnsZoneId"] for item in resources}:
            prefix = (
                zone_id
                + "/providers/Microsoft.Authorization/roleAssignments/"
            )
            assignments = az(
                "role",
                "assignment",
                "list",
                "--scope",
                zone_id,
                "--fill-principal-name",
                "false",
            )
            for assignment in assignments:
                role_id = assignment.get("id", "")
                if (
                    assignment.get("scope", "").lower()
                    == zone_id.lower()
                    and assignment.get("principalId", "").lower()
                    in principals
                    and assignment.get("roleDefinitionId", "")
                    .lower()
                    .endswith(
                        "/befefa01-2a29-4197-83a8-272ff33ce314"
                    )
                    and role_id.lower().startswith(prefix.lower())
                    and self.guid(role_id[len(prefix) :])
                ):
                    az("role", "assignment", "delete", "--ids", role_id)
        return ingress_group

    def delete(
        self,
        parameters,
        request,
        saved,
        az,
        resource_id,
        group,
        owned,
        wait_deleted,
        provider_error,
    ):
        inventories = self.saved_inventories(
            parameters, saved, resource_id, provider_error,
        )
        retiring = saved.get("retiringIngress", [])
        if not isinstance(retiring, list):
            self.fail(
                "invalid-state",
                "Saved retiring Azure ingress inventory must be an array",
            )
        if retiring:
            pending = {"managedIngress": retiring}
            for inventory in self.saved_inventories(
                parameters, pending, resource_id, provider_error,
            ):
                if inventory not in inventories:
                    inventories.append(inventory)
        claims = self.saved_claims(
            parameters, saved, resource_id, provider_error,
        )
        if not inventories and not claims:
            saved["managedIngress"] = []
            saved["retiringIngress"] = []
            saved["managedIngressClaims"] = []
            return
        for zone_id in {
            item["dnsZoneId"] for item in [*inventories, *claims]
        }:
            self.validate_zone_scope(
                self.zone(zone_id), parameters, "invalid-state",
            )
        records = {
            record
            for item in [*inventories, *claims]
            for record in item["dnsRecordSetIds"]
        }
        roles = {
            role
            for inventory in inventories
            for role in (
                inventory.get("dnsRoleAssignmentId"),
                inventory.get("publicIpRoleAssignmentId"),
            )
            if role
        }
        for record in sorted(records):
            self.delete_record(record, request, az, provider_error)
        for role in sorted(roles):
            inventory = next(
                item
                for item in inventories
                if role
                in (
                    item.get("dnsRoleAssignmentId"),
                    item.get("publicIpRoleAssignmentId"),
                )
            )
            self.delete_role(inventory, role, az)
        ingress_group = self.delete_claim_roles(
            parameters,
            request,
            [*inventories, *claims],
            az,
            group,
            owned,
        )
        if ingress_group:
            if (
                ingress_group.get("properties", {}).get("provisioningState")
                != "Deleting"
            ):
                az(
                    "group",
                    "delete",
                    "--subscription",
                    parameters["subscriptionId"],
                    "--name",
                    parameters["ingressResourceGroup"],
                    "--yes",
                    "--no-wait",
                )
            wait_deleted(
                lambda: group(
                    parameters,
                    parameters["ingressResourceGroup"],
                ),
                request,
                "Ingress resource group",
                1800,
                lambda resource: resource.get("properties", {}).get(
                    "provisioningState"
                ),
            )
        saved["managedIngress"] = []
        saved["retiringIngress"] = []
        saved["managedIngressClaims"] = []
