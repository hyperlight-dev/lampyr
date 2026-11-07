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
param lampyrSourceDigest string

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
      enabled: eventingEnabled
    }
    securityProfile: {
      workloadIdentity: {
        enabled: eventingEnabled
      }
    }
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
  eventing: eventingEnabled ? union(eventing!.outputs.platform, {
    resourceGroup: eventingResourceGroup
    publisherResourceGroup: resourceGroup().name
    publisherRoleAssignments: publisherAccess!.outputs.roleAssignments
  }) : {}
}
