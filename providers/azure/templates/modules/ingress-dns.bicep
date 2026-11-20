param installationId string
param zoneName string
param recordName string
param principalId string

resource zone 'Microsoft.Network/dnsZones@2018-05-01' existing = {
  name: zoneName
}

var wildcardRecord = empty(recordName) ? '*' : '*.${recordName}'
var defaultWildcardRecord = empty(recordName) ? '*.default' : '*.default.${recordName}'

resource dnsAccess 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (!empty(principalId)) {
  name: guid(zone.id, installationId, 'cert-manager')
  scope: zone
  properties: {
    principalId: principalId
    principalType: 'ServicePrincipal'
    roleDefinitionId: subscriptionResourceId(
      'Microsoft.Authorization/roleDefinitions',
      'befefa01-2a29-4197-83a8-272ff33ce314'
    )
  }
}

output zoneId string = zone.id
output recordSetIds array = [for name in [
  wildcardRecord
  defaultWildcardRecord
]: resourceId('Microsoft.Network/dnsZones/A', zoneName, name)]
output roleAssignmentId string = empty(principalId) ? '' : dnsAccess!.id
