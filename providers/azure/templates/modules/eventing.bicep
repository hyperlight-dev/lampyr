param installationId string
param location string
param oidcIssuer string
param clusterPrincipalId string
param kubeletPrincipalId string

var prefix = 'lampyr-${take(installationId, 12)}'
var tags = {
  'lampyr-installation-id': installationId
}

resource publicIP 'Microsoft.Network/publicIPAddresses@2024-05-01' = {
  name: '${prefix}-webhook'
  location: location
  tags: tags
  sku: {
    name: 'Standard'
    tier: 'Regional'
  }
  properties: {
    publicIPAllocationMethod: 'Static'
    dnsSettings: {
      domainNameLabel: '${prefix}-${uniqueString(resourceGroup().id)}'
    }
  }
}

resource frontDoor 'Microsoft.Cdn/profiles@2024-02-01' = {
  name: prefix
  location: 'global'
  tags: tags
  sku: {
    name: 'Standard_AzureFrontDoor'
  }
}

resource endpoint 'Microsoft.Cdn/profiles/afdEndpoints@2024-02-01' = {
  parent: frontDoor
  name: prefix
  location: 'global'
  tags: tags
  properties: {
    enabledState: 'Enabled'
  }
}

resource originGroup 'Microsoft.Cdn/profiles/originGroups@2024-02-01' = {
  parent: frontDoor
  name: 'aks'
  properties: {
    sessionAffinityState: 'Disabled'
    loadBalancingSettings: {
      sampleSize: 4
      successfulSamplesRequired: 3
      additionalLatencyInMilliseconds: 50
    }
    healthProbeSettings: {
      probePath: '/healthz'
      probeRequestType: 'GET'
      probeProtocol: 'Http'
      probeIntervalInSeconds: 30
    }
  }
}

resource origin 'Microsoft.Cdn/profiles/originGroups/origins@2024-02-01' = {
  parent: originGroup
  name: 'aks'
  properties: {
    hostName: publicIP.properties.dnsSettings.fqdn
    originHostHeader: publicIP.properties.dnsSettings.fqdn
    httpPort: 80
    httpsPort: 443
    priority: 1
    weight: 1000
    enabledState: 'Enabled'
  }
}

resource route 'Microsoft.Cdn/profiles/afdEndpoints/routes@2024-02-01' = {
  parent: endpoint
  name: 'eventgrid'
  properties: {
    originGroup: {
      id: originGroup.id
    }
    supportedProtocols: ['Http', 'Https']
    patternsToMatch: ['/*']
    forwardingProtocol: 'HttpOnly'
    linkToDefaultDomain: 'Enabled'
    httpsRedirect: 'Enabled'
    enabledState: 'Enabled'
  }
  dependsOn: [origin]
}

resource controller 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: '${prefix}-controller'
  location: location
  tags: tags
}

resource fiberd 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: '${prefix}-fiberd'
  location: location
  tags: tags
}

resource controllerFederation 'Microsoft.ManagedIdentity/userAssignedIdentities/federatedIdentityCredentials@2023-01-31' = {
  parent: controller
  name: 'kubernetes'
  properties: {
    issuer: oidcIssuer
    subject: 'system:serviceaccount:lampyr-system:eventgrid-controller'
    audiences: ['api://AzureADTokenExchange']
  }
}

resource fiberdFederation 'Microsoft.ManagedIdentity/userAssignedIdentities/federatedIdentityCredentials@2023-01-31' = {
  parent: fiberd
  name: 'kubernetes'
  properties: {
    issuer: oidcIssuer
    subject: 'system:serviceaccount:default:fiberd-grant'
    audiences: ['api://AzureADTokenExchange']
  }
}

resource registry 'Microsoft.ContainerRegistry/registries@2023-07-01' = {
  name: 'lampyr${uniqueString(resourceGroup().id, installationId)}'
  location: location
  tags: tags
  sku: {
    name: 'Basic'
  }
  properties: {
    adminUserEnabled: false
    publicNetworkAccess: 'Enabled'
  }
}

var pullRole = subscriptionResourceId('Microsoft.Authorization/roleDefinitions', '7f951dda-4ed3-4680-a7ca-43fe172d538d')
resource kubeletPull 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(registry.id, pullRole, kubeletPrincipalId)
  scope: registry
  properties: {
    principalId: kubeletPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: pullRole
  }
}
resource fiberdPull 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(registry.id, pullRole, fiberd.id)
  scope: registry
  properties: {
    principalId: fiberd.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: pullRole
  }
}
resource controllerAccess 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(resourceGroup().id, installationId, controller.id)
  properties: {
    principalId: controller.properties.principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', '1e241071-0855-49ea-94dc-649edcd759de')
  }
}
resource clusterNetwork 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(resourceGroup().id, installationId, clusterPrincipalId)
  properties: {
    principalId: clusterPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId('Microsoft.Authorization/roleDefinitions', '4d97b98b-1d4f-4787-a291-c67834d212e7')
  }
}

output controllerPrincipalId string = controller.properties.principalId
output platform object = {
  resourceGroupId: resourceGroup().id
  publicIpId: publicIP.id
  publicIpName: publicIP.name
  webhookHost: endpoint.properties.hostName
  frontDoorId: frontDoor.properties.frontDoorId
  controllerIdentityId: controller.id
  controllerPrincipalId: controller.properties.principalId
  controllerClientId: controller.properties.clientId
  fiberdIdentityId: fiberd.id
  fiberdClientId: fiberd.properties.clientId
  registryId: registry.id
  registryLoginServer: registry.properties.loginServer
}
