param installationId string
param location string
param clusterPrincipalId string
param oidcIssuer string
param workloadIdentityEnabled bool

var tags = {
  'lampyr-installation-id': installationId
}

resource publicIp 'Microsoft.Network/publicIPAddresses@2024-05-01' = {
  name: 'lampyr-ingress-${take(installationId, 12)}'
  location: location
  tags: tags
  sku: {
    name: 'Standard'
  }
  properties: {
    publicIPAllocationMethod: 'Static'
  }
}

resource clusterAccess 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(resourceGroup().id, installationId, 'cluster-ingress')
  properties: {
    principalId: clusterPrincipalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      '4d97b98b-1d4f-4787-a291-c67834d212e7'
    )
  }
}

resource dnsIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = if (workloadIdentityEnabled) {
  name: 'lampyr-ingress-dns-${take(installationId, 12)}'
  location: location
  tags: tags
}

resource dnsFederation 'Microsoft.ManagedIdentity/userAssignedIdentities/federatedIdentityCredentials@2023-01-31' = if (workloadIdentityEnabled) {
  parent: dnsIdentity
  name: 'cert-manager'
  properties: {
    audiences: [
      'api://AzureADTokenExchange'
    ]
    issuer: oidcIssuer
    subject: 'system:serviceaccount:cert-manager:cert-manager'
  }
}

output resourceGroupId string = resourceGroup().id
output publicIpAddress string = publicIp.properties.ipAddress
output publicIpId string = publicIp.id
output publicIpName string = publicIp.name
output publicIpRoleAssignmentId string = clusterAccess.id
output dnsIdentityId string = workloadIdentityEnabled ? dnsIdentity!.id : ''
output dnsIdentityClientId string = workloadIdentityEnabled ? dnsIdentity!.properties.clientId : ''
output dnsIdentityPrincipalId string = workloadIdentityEnabled ? dnsIdentity!.properties.principalId : ''
