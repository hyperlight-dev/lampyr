targetScope = 'resourceGroup'

param installationId string
param tenantId string
param subscriptionId string
param resourceGroupName string
param clusterName string
param location string
param kubernetesVersion string
param nodeVmSize string
@minValue(1)
@maxValue(100)
param nodeCount int
param eventingEnabled bool
param eventingResourceGroup string
param ingressResourceGroup string
param lampyrSourceDigest string
param ingressDnsEnabled bool = false
param ingressAcmeEnabled bool = false
param ingressWorkloadIdentityEnabled bool = false
param clusterOidcIssuerEnabled bool = false
param clusterWorkloadIdentityEnabled bool = false
param ingressDnsSubscriptionId string = ''
param ingressDnsResourceGroup string = ''
param ingressDnsZone string = ''
param ingressDnsRecord string = ''

var tags = {
  'lampyr-installation-id': installationId
}

resource cluster 'Microsoft.ContainerService/managedClusters@2025-07-01' = {
  name: clusterName
  location: location
  tags: tags
  identity: {
    type: 'SystemAssigned'
  }
  properties: {
    dnsPrefix: clusterName
    kubernetesVersion: kubernetesVersion
    enableRBAC: true
    agentPoolProfiles: [
      {
        name: 'nodepool1'
        count: nodeCount
        mode: 'System'
        osSKU: 'Ubuntu2404'
        osType: 'Linux'
        type: 'VirtualMachineScaleSets'
        vmSize: nodeVmSize
      }
    ]
    networkProfile: {
      networkPlugin: 'azure'
      networkPluginMode: 'overlay'
      networkDataplane: 'cilium'
      networkPolicy: 'cilium'
    }
    oidcIssuerProfile: {
      enabled: eventingEnabled || ingressWorkloadIdentityEnabled || clusterOidcIssuerEnabled
    }
    securityProfile: {
      workloadIdentity: {
        enabled: eventingEnabled || ingressWorkloadIdentityEnabled || clusterWorkloadIdentityEnabled
      }
    }
  }
}

module ingressInfrastructure 'modules/ingress.bicep' = if (ingressDnsEnabled || ingressWorkloadIdentityEnabled) {
  name: 'lampyr-ingress-${take(installationId, 12)}'
  scope: resourceGroup(ingressResourceGroup)
  params: {
    installationId: installationId
    location: location
    clusterPrincipalId: cluster.identity.principalId!
    oidcIssuer: ingressWorkloadIdentityEnabled ? cluster.properties.oidcIssuerProfile.issuerURL! : ''
    workloadIdentityEnabled: ingressWorkloadIdentityEnabled
  }
}

module ingressDns 'modules/ingress-dns.bicep' = if (ingressDnsEnabled) {
  name: 'lampyr-ingress-dns-${take(installationId, 12)}'
  scope: resourceGroup(ingressDnsSubscriptionId, ingressDnsResourceGroup)
  params: {
    installationId: installationId
    zoneName: ingressDnsZone
    recordName: ingressDnsRecord
    principalId: ingressAcmeEnabled ? ingressInfrastructure!.outputs.dnsIdentityPrincipalId : ''
  }
}

module eventing 'modules/eventing.bicep' = if (eventingEnabled) {
  name: 'lampyr-eventing-${take(installationId, 12)}'
  scope: resourceGroup(eventingResourceGroup)
  params: {
    installationId: installationId
    location: location
    oidcIssuer: cluster.properties.oidcIssuerProfile.issuerURL
    clusterPrincipalId: cluster.identity.principalId!
    kubeletPrincipalId: cluster.properties.identityProfile.kubeletidentity.objectId
  }
}

module publisherAccess 'modules/publisher-access.bicep' = if (eventingEnabled) {
  name: 'lampyr-publisher-${take(installationId, 12)}'
  params: {
    installationId: installationId
    principalId: eventing!.outputs.controllerPrincipalId
  }
}

output lampyr object = {
  installationId: installationId
  sourceDigest: lampyrSourceDigest
  tenantId: tenantId
  subscriptionId: subscriptionId
  clusterId: cluster.id
  clusterName: cluster.name
  resourceGroup: resourceGroupName
  ingress: ingressDnsEnabled ? {
    resourceGroup: ingressResourceGroup
    resourceGroupId: ingressInfrastructure!.outputs.resourceGroupId
    publicIpAddress: ingressInfrastructure!.outputs.publicIpAddress
    publicIpId: ingressInfrastructure!.outputs.publicIpId
    publicIpName: ingressInfrastructure!.outputs.publicIpName
    publicIpRoleAssignmentId: ingressInfrastructure!.outputs.publicIpRoleAssignmentId
    clusterPrincipalId: cluster.identity.principalId!
    dnsZoneId: ingressDns!.outputs.zoneId
    dnsRecordSetIds: ingressDns!.outputs.recordSetIds
    dnsRoleAssignmentId: ingressDns!.outputs.roleAssignmentId
    dnsIdentityId: ingressAcmeEnabled ? ingressInfrastructure!.outputs.dnsIdentityId : ''
    dnsIdentityClientId: ingressAcmeEnabled ? ingressInfrastructure!.outputs.dnsIdentityClientId : ''
    dnsIdentityPrincipalId: ingressAcmeEnabled ? ingressInfrastructure!.outputs.dnsIdentityPrincipalId : ''
  } : {}
  eventing: eventingEnabled ? union(eventing!.outputs.platform, {
    resourceGroup: eventingResourceGroup
    publisherResourceGroup: resourceGroup().name
    publisherRoleAssignments: publisherAccess!.outputs.roleAssignments
  }) : {}
}
